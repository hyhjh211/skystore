/*
 * FastAPI
 *
 * No description provided (generated by Openapi Generator https://github.com/openapitools/openapi-generator)
 *
 * The version of the OpenAPI document: 0.1.0
 * 
 * Generated by: https://openapi-generator.tech
 */




#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteObjectsRequest {
    #[serde(rename = "bucket")]
    pub bucket: String,
    #[serde(rename = "keys")]
    pub keys: Vec<String>,
}

impl DeleteObjectsRequest {
    pub fn new(bucket: String, keys: Vec<String>) -> DeleteObjectsRequest {
        DeleteObjectsRequest {
            bucket,
            keys,
        }
    }
}


