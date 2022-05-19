use crate::prelude::CheapClone;
use anyhow::Error;
use bytes::Bytes;
use futures03::Stream;
use http::header::CONTENT_LENGTH;
use http::Uri;
use reqwest::multipart;
use serde::{Deserialize};
use std::time::Duration;
use std::{str::FromStr, sync::Arc};

use cid;
use cid::multihash::MultihashDigest;
use std::env;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatApi {
    Block,
    Files,
}

impl StatApi {
    fn route(&self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Files => "files",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BlockStatResponse {
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FilesStatResponse {
    cumulative_size: u64,
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
    tolkien_url: String,
}

impl CheapClone for IpfsClient {
    fn cheap_clone(&self) -> Self {
        IpfsClient {
            base: self.base.cheap_clone(),
            client: self.client.cheap_clone(),
            tolkien_url: self.tolkien_url.clone(),
            //mongo: self.mongo.clone(),
            //s3: self.s3.clone(),
        }
    }
}

impl IpfsClient {
    pub fn new(base: &str) -> Result<Self, Error> {

        Ok(IpfsClient {
            client: Arc::new(reqwest::Client::new()),
            base: Arc::new(Uri::from_str(base)?),
            tolkien_url: match env::var("TOLKIEN_URL") {
                Ok(x) => {x}
                Err(_) => {
                    println!("No Tolkien Url! defaulting to http://127.0.0.1:30000");
                    String::from("http://127.0.0.1:30000")
                }
            }
        })
    }

    pub fn localhost() -> Self {

        IpfsClient {
            client: Arc::new(reqwest::Client::new()),
            base: Arc::new(Uri::from_str("http://localhost:5001").unwrap()),
            tolkien_url: match env::var("TOLKIEN_URL"){
                Ok(x) => {x}
                Err(_) => {String::from("http://127.0.0.1:30000")}
            },
        }
    }

    /// Calls stat for the given API route, and returns the total size of the object.
    pub async fn stat_size(
        &self,
        api: StatApi,
        mut cid: String,
        timeout: Duration,
    ) -> Result<u64, reqwest::Error> {
        let route = format!("{}/stat", api.route());
        if api == StatApi::Files {
            // files/stat requires a leading `/ipfs/`.
            cid = format!("/ipfs/{}", cid);
        }
        let url = self.url(&route, cid);
        let res = self.call(url, None, Some(timeout)).await?;
        match api {
            StatApi::Files => Ok(res.json::<FilesStatResponse>().await?.cumulative_size),
            StatApi::Block => Ok(res.json::<BlockStatResponse>().await?.size),
        }
    }

    //actually send the tolkien http request - wrapped in a separate function so that error types are easier to deal with.
    pub async fn tolkien_call(&self, c_id: &String, timeout: Option<Duration>) -> Result<reqwest::Response,reqwest::Error> {
        let mut a =self.client.get(format!("{}{}{}",self.tolkien_url,"/api/ipfs/",c_id));
        if let Some(x) = timeout {
            a = a.timeout(x);
        }
        let b= a.send().await?.error_for_status()?;
        return Ok(b);
    }

    //Queries Tolkien via https
    pub async fn tolkien_cat(&self, c_id: &String, timeout: Duration) -> Result<Bytes,anyhow::Error> {

        let ipfs_cid = cid::CidGeneric::<64>::from_str(c_id)?;

        if ipfs_cid.codec() != 0x55 {
            return Err(anyhow::anyhow!("Codec other than RAW not supported."));
        }

        let res = self.tolkien_call(c_id,Some(timeout,));
        //^ this starts the call after that preliminary check - where most invalid should fail.

        let mh = ipfs_cid.hash();

        let code_result = cid::multihash::Code::try_from(mh.code());

        let code_val = match code_result{
            Ok(c) => {c},
            Err(_) => {return Err(anyhow::anyhow!("Failed to parse hash's code"));}
        };

        let res_string = match res.await {
            Ok(x)=>{
                x.text().await?
            }
            Err(_) => {
                return Err(anyhow::anyhow!("Failed to get "));
            }
        };

        let digest = code_val.digest(res_string.as_bytes()); //no idea why clion isn't showing up the typing here?

        return if digest.eq(mh) {
            Ok(Bytes::from(res_string))
        }
        else{
            Err(anyhow::anyhow!("Hashes do not match."))
        };
    }

    /// Download the entire contents.
    pub async fn cat_all(&self, cid: String, timeout: Duration) -> Result<Bytes, reqwest::Error> {
        self.call(self.url("cat", cid), None, Some(timeout))
            .await?
            .bytes()
            .await
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

    pub async fn get_block(&self, cid: String) -> Result<Bytes, reqwest::Error> {
        let form = multipart::Form::new().part("arg", multipart::Part::text(cid));
        self.call(format!("{}api/v0/block/get", self.base), Some(form), None)
            .await?
            .bytes()
            .await
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

    fn url(&self, route: &str, arg: String) -> String {
        // URL security: We control the base and the route, user-supplied input goes only into the
        // query parameters.
        format!("{}api/v0/{}?arg={}", self.base, route, arg)
    }

    pub async fn call(
        &self,
        url: String,
        form: Option<multipart::Form>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self.client.get(&url);
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
