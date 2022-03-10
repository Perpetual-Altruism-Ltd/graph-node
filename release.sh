#!/bin/bash

abort () {
  local ERROR_MESSAGE=$1
  echo "Release failed, error message:"
  echo $ERROR_MESSAGE
  exit 1
}

# Returns 0 if lines are unique, or 1 otherwise
lines_uniq () {
  local LINES=$1
  if [ $(echo $LINES | uniq | wc -l) -ne 1 ]; then
    return 0
  else
    return 1
  fi
}

ALL_TOML_FILE_NAMES=$(find **/Cargo.toml | grep -v integration-tests)

echo $ALL_TOML_FILE_NAMES

ALL_TOML_VERSIONS=$(echo $ALL_TOML_FILE_NAMES | xargs cat | grep '^version = ' | awk '{print $3}')

echo $ALL_TOML_VERSIONS

# Asserts all .toml versions are currently the same, otherwise abort.
if $(lines_uniq ALL_TOML_VERSIONS); then
  abort "Some Cargo.toml files have different versions than others, make sure they're all the same before creating a new release"
fi

# 2. Save current version in variable
CURRENT_VERSION=$(echo $ALL_TOML_VERSIONS | awk '{print $1}' | tr -d '"')

echo $CURRENT_VERSION

# 3. Increment by CLI argument (major, minor, patch)

MAJOR=$(echo $CURRENT_VERSION | cut -d. -f1)
MINOR=$(echo $CURRENT_VERSION | cut -d. -f2)
PATCH=$(echo $CURRENT_VERSION | cut -d. -f3)

echo $MAJOR
echo $MINOR
echo $PATCH

case $1 in

  "major")
    let "MAJOR++"
    MINOR=0
    PATCH=0
    ;;

  "minor")
    let "MINOR++"
    PATCH=0
    ;;

  "patch")
    let "PATCH++"
    ;;

  *)
    abort "Version argument should be one of: major, minor or patch"
    ;;
esac

echo $MAJOR
echo $MINOR
echo $PATCH

# 4. Replace on all .toml files

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"

# MacOS/OSX unfortunately doesn't have the same API for `sed`, so we're
# using `perl` instead since it's installed by default in all Mac machines.
#
# For mor info: https://stackoverflow.com/questions/4247068/sed-command-with-i-option-failing-on-mac-but-works-on-linux
if [[ "$OSTYPE" == "darwin"* ]]; then
    perl -i -pe"s/^version = \"${CURRENT_VERSION}\"/version = \"${NEW_VERSION}\"/" $ALL_TOML_FILE_NAMES
# Default, for decent OSs (eg: GNU-Linux)
else
    sed -i "s/^version = \"${CURRENT_VERSION}\"/version = \"${NEW_VERSION}\"/" $ALL_TOML_FILE_NAMES
fi


# 5. Assert all the new .toml versions are the same, otherwise abort
# TODO

# 6. Cargo build to change .lock file
cargo build --tests

# 7. Assert .lock file changed the versions, otherwise abort
# TODO
