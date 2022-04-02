use crate::prelude::CheapClone;
use anyhow::Error;
use bytes::Bytes;
use futures03::Stream;
use http::header::CONTENT_LENGTH;
use http::Uri;
use reqwest::multipart;
use serde::{Deserialize,Serialize};
use std::time::Duration;
use std::{str::FromStr, sync::Arc};

use std::env;
use s3;
//use futures::stream::TryStreamExt;
use mongodb::{bson::doc};

use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
//use s3::S3Error; // Honestly no idea if these are right- I thiiiiink these should be rust-s3 and there should just be rust-s3 (or s3, one of the two) in the cargo.toml might have to do "extern crate s3" somewhere.
use std::str;


#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ObjectStatResponse {
    pub hash: String,
    pub num_links: u64,
    pub block_size: u64,
    pub links_size: u64,
    pub data_size: u64,
    pub cumulative_size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AddResponse {
    pub name: String,
    pub hash: String,
    pub size: String,
}

#[derive(Clone)]
pub struct IpfsClient {
    base: Arc<Uri>,
    client: Arc<reqwest::Client>,
    mongo: mongodb::Client,
    s3: Arc<Bucket>,
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(non_snake_case)]
pub struct FileModel{
    pub fileId: String,
    pub ipfsUrl: String,
    pub s3Url: String,
    pub fileType: String,
}

#[allow(dead_code)]
pub struct Storage {
    name: String,
    region: Region,
    credentials: Credentials,
    bucket: String,
    location_supported: bool,
}

impl CheapClone for IpfsClient {
    fn cheap_clone(&self) -> Self {
        IpfsClient {
            base: self.base.cheap_clone(),
            client: self.client.cheap_clone(),
            mongo: self.mongo.clone(), // this should maybe be cheap_clone but i have no idea how that's implemented.
            s3: self.s3.clone(),
        }
    }
}

impl IpfsClient {
    pub fn new(base: &str) -> Result<Self, Error> {

        let aws = Storage {
            name: "aws".into(),
            region: (&env::var("AWS_REGION_NAME").unwrap()).parse()?,
            credentials: Credentials::from_env_specific(
                Some("AWS_ACCESS_KEY_ID"),
                Some("AWS_SECRET_ACCESS_KEY"),
                None,
                None,
            )?,
                bucket: (&env::var("AWS_BUCKET_NAME").unwrap()).to_string(),
        location_supported: true,
        };
        let bucket = Bucket::new(&aws.bucket, aws.region, aws.credentials)?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let res = rt.block_on(async { mongodb::Client::with_uri_str(&env::var("MONGO_LINK").unwrap()).await.unwrap() });

        Ok(IpfsClient {
            client: Arc::new(reqwest::Client::new()),
            base: Arc::new(Uri::from_str(base)?),
            mongo: res, // NOTE: this means the MONGO_LINK environment variable has to be set.
            s3: Arc::new(bucket), // No, I'm not sure this is the right way to do this.
        })
    }

    pub fn localhost() -> Self {
        let aws = Storage {
            name: "aws".into(),
            region: (&env::var("AWS_REGION_NAME").unwrap()).parse().unwrap(),
            credentials: Credentials::from_env_specific(
                Some("AWS_ACCESS_KEY_ID"),
                Some("AWS_SECRET_ACCESS_KEY"),
                None,
                None,
            ).unwrap(),
            bucket: (&env::var("AWS_BUCKET_NAME").unwrap()).to_string(),
            location_supported: true,
        };
        let bucket = Bucket::new(&aws.bucket, aws.region, aws.credentials).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let res = rt.block_on(async { mongodb::Client::with_uri_str("mongodb://localhost:27017").await.unwrap() });

        IpfsClient {
            client: Arc::new(reqwest::Client::new()),
            base: Arc::new(Uri::from_str("http://localhost:5001").unwrap()),
            mongo: res,
            s3: Arc::new(bucket),
        }
    }

    /// Calls `object stat`.
    pub async fn object_stat(
        &self,
        path: String,
        timeout: Duration,
    ) -> Result<ObjectStatResponse, reqwest::Error> {
        self.call(self.url("object/stat", path), None, Some(timeout))
            .await?
            .json()
            .await
    }

    //Download contents from s3 cache, throw if unable to locate contents
    pub async fn s3_cat(&self, cid: &String) -> Result<Bytes,anyhow::Error> {
        let database = self.mongo.database(&env::var("MONGO_DATABASE").unwrap());//TODO: FILL THESE OUT *should probably read from env
        let collection = database.collection::<FileModel>(&env::var("MONGO_COLLECTION").unwrap());

        let filter = doc!{"ipfsUrl":&cid};

        let mongo_res = collection.find_one(filter,None).await;
        let fil = match mongo_res{
            Ok(x)=>x,
            Err(_)=> {
                return Err(anyhow::anyhow!("Mongo Failed"));
            },
        };
        return match fil{
            Some(x)=> {
                let (data, code) = self.s3.get_object(x.s3Url).await?;
                let ret;
                if code == 200 {
                    ret =Ok(bytes::Bytes::from(data));
                }
                else{
                    ret =Err(anyhow::anyhow!("s3 failed"));
                }
                ret
            }
            None => {Err(anyhow::anyhow!("No record in Mongo"))}
        }


    }

    /// Download the entire contents.
    pub async fn cat_all(&self, cid: String, timeout: Duration) -> Result<Bytes, reqwest::Error> {

        return match self.s3_cat(&cid).await{
            Ok(x)=>{
                Ok(x)
            }
            Err(_)=>{
                Ok(self.call(self.url("cat", cid), None, Some(timeout))
                .await?
                .bytes()
                .await?)
            }
        }

        //todo: figure out if this is right.



        //so really this should be last case~ I'll just leave it here I guess? so if it doesn't return already, then this runs. which shouuuld be fine. Eventually I should rewrite to use the ipfs-crate.

    }

    pub async fn cat(
        &self,
        cid: String,
    ) -> Result<impl Stream<Item = Result<Bytes, reqwest::Error>>, reqwest::Error> {
        Ok(self
            .call(self.url("cat", cid), None, None)
            .await?
            .bytes_stream())
    }

    pub async fn test(&self) -> Result<(), reqwest::Error> {
        self.call(format!("{}api/v0/version", self.base), None, None)
            .await
            .map(|_| ())
    }

    pub async fn add(&self, data: Vec<u8>) -> Result<AddResponse, reqwest::Error> {
        let form = multipart::Form::new().part("path", multipart::Part::bytes(data));

        self.call(format!("{}api/v0/add", self.base), Some(form), None)
            .await?
            .json()
            .await
    }

    fn url(&self, route: &'static str, arg: String) -> String {
        // URL security: We control the base and the route, user-supplied input goes only into the
        // query parameters.
        format!("{}api/v0/{}?arg={}", self.base, route, arg)
    }

    async fn call(
        &self,
        url: String,
        form: Option<multipart::Form>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.post(&url);
        if let Some(form) = form {
            req = req.multipart(form);
        } else {
            // Some servers require `content-length` even for an empty body.
            req = req.header(CONTENT_LENGTH, 0);
        }

        if let Some(timeout) = timeout {
            req = req.timeout(timeout)
        }

        req.send()
            .await
            .map(|res| res.error_for_status())
            .and_then(|x| x)
    }
}
