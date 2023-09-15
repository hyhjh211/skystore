use crate::objstore_client::ObjectStoreClient;
use crate::stream_utils::split_streaming_blob;
use crate::type_utils::*;

use s3s::dto::*;
use s3s::stream::ByteStream;
use s3s::{S3Request, S3Response, S3Result, S3};

use chrono::Utc;
use skystore_rust_client::apis::configuration::Configuration;
use skystore_rust_client::apis::default_api as apis;
use skystore_rust_client::models;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::error;

pub struct SkyProxy {
    store_clients: HashMap<String, Arc<Box<dyn ObjectStoreClient>>>,
    dir_conf: Configuration,
    client_from_region: String,
}

impl SkyProxy {
    pub async fn new(regions: Vec<String>, client_from_region: String) -> Self {
        let mut store_clients = HashMap::new();

        //// Test Configuration, hard coded from conf.py
        // let regions = vec![
        //     "aws:us-west-1",
        //     "aws:us-east-2",
        //     "gcp:us-west1",
        //     "aws:eu-central-1",
        // ];

        // Local test configuration
        for r in regions {
            let split: Vec<&str> = r.splitn(2, ':').collect();
            let (_, region) = (split[0], split[1]);

            let client = Arc::new(Box::new(
                crate::client_impls::s3::S3ObjectStoreClient::new(
                    "http://localhost:8014".to_string(),
                )
                .await,
            ) as Box<dyn ObjectStoreClient>);

            store_clients.insert(r.to_string(), client.clone());

            // if bucket not exists, create one
            let skystore_bucket_name = format!("skystore-{}", region);
            match client
                .create_bucket(S3Request::new(new_create_bucket_request(
                    skystore_bucket_name,
                    Some(r),
                )))
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    if e.to_string().contains("BucketAlreadyExists") {
                    } else {
                        panic!("Failed to create bucket: {}", e);
                    }
                }
            };
        }

        // Real object store configuration (regions: init regions)
        // for r in regions {
        //     let split: Vec<&str> = r.splitn(2, ':').collect();
        //     let (provider, region) = (split[0], split[1]);

        //     let client: Box<dyn ObjectStoreClient> = match provider {
        //         "azure" => {
        //             Box::new(crate::client_impls::azure::AzureObjectStoreClient::new().await)
        //         }
        //         "gcp" => Box::new(crate::client_impls::gcp::GCPObjectStoreClient::new().await),
        //         "aws" => Box::new(
        //             crate::client_impls::s3::S3ObjectStoreClient::new(format!(
        //                 "https://s3.{}.amazonaws.com",
        //                 region
        //             ))
        //             .await,
        //         ),
        //         _ => panic!("Unknown provider: {}", provider),
        //     };

        //     let client_arc = Arc::new(client);
        //     store_clients.insert(r.to_string(), client_arc.clone());

        //     // TODO: might need to fix this, create bucket in INIT_REGIONS at startup in the dataplane for now
        //     let skystore_bucket_name = format!("skystore-{}", region);
        //     let res = client_arc
        //         .head_bucket(S3Request::new(new_head_bucket_request(
        //             skystore_bucket_name.clone(),
        //         )))
        //         .await;

        //     match res {
        //         Ok(_) => (),
        //         Err(_) => {
        //             let bucket_region = if provider == "aws" {
        //                 Some(region.to_string())
        //             } else {
        //                 None
        //             };
        //             let _ = client_arc
        //                 .create_bucket(S3Request::new(new_create_bucket_request(
        //                     skystore_bucket_name,
        //                     bucket_region,
        //                 )))
        //                 .await
        //                 .unwrap();
        //         }
        //     }
        // }

        //// Demo Configuration
        // store_clients.insert(
        //     "azure:westus3".to_string(),
        //     Arc::new(
        //         Box::new(crate::client_impls::azure::AzureObjectStoreClient::new().await)
        //             as Box<dyn ObjectStoreClient>,
        //     ),
        // );
        // store_clients.insert(
        //     "gcp:us-west1".to_string(),
        //     Arc::new(
        //         Box::new(crate::client_impls::gcp::GCPObjectStoreClient::new().await)
        //             as Box<dyn ObjectStoreClient>,
        //     ),
        // );
        // store_clients.insert(
        //     "aws:us-west-2".to_string(),
        //     Arc::new(Box::new(
        //         crate::client_impls::s3::S3ObjectStoreClient::new(
        //             "https://s3.us-west-2.amazonaws.com".to_string(),
        //         )
        //         .await,
        //     ) as Box<dyn ObjectStoreClient>),
        // );

        let dir_conf = Configuration {
            base_path: "http://localhost:3000".to_string(),
            ..Default::default()
        };

        apis::healthz(&dir_conf)
            .await
            .expect("directory service not healthy");

        Self {
            store_clients,
            dir_conf,
            client_from_region,
        }
    }
}

// Needed for S3 trait
impl std::fmt::Debug for SkyProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkyProxy").finish()
    }
}

impl SkyProxy {
    async fn locate_object(
        &self,
        bucket: String,
        key: String,
    ) -> Result<Option<models::LocateObjectResponse>, s3s::S3Error> {
        let locate_resp = apis::locate_object(
            &self.dir_conf,
            models::LocateObjectRequest {
                bucket,
                key,
                client_from_region: self.client_from_region.clone(),
            },
        )
        .await;

        match locate_resp {
            Ok(locator) => Ok(Some(locator)),
            Err(error) => {
                let is_404 = locate_response_is_404(&error);

                if is_404 {
                    Ok(None)
                } else {
                    Err(s3s::S3Error::with_message(
                        s3s::S3ErrorCode::InternalError,
                        "locate_object failed",
                    ))
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl S3 for SkyProxy {
    ///////////////////////////////////////////////////////////////////////////
    /// We start with the "blanket impl" that don't need actual operation
    /// from the physical store clients.
    #[tracing::instrument(level = "info")]
    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let v2 = self
            .list_objects_v2(req.map_input(Into::into))
            .await?
            .output;

        let output = ListObjectsOutput {
            contents: v2.contents,
            delimiter: v2.delimiter,
            encoding_type: v2.encoding_type,
            name: v2.name,
            prefix: v2.prefix,
            max_keys: v2.max_keys,
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    #[tracing::instrument(level = "info")]
    async fn abort_multipart_upload(
        &self,
        _req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        return Ok(S3Response::new(AbortMultipartUploadOutput::default()));
    }
    ///////////////////////////////////////////////////////////////////////////

    #[tracing::instrument(level = "info")]
    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        // Send start create bucket request
        let create_bucket_resp = apis::start_create_bucket(
            &self.dir_conf,
            models::CreateBucketRequest {
                bucket: req.input.bucket.clone(),
                client_from_region: self.client_from_region.clone(),
                warmup_regions: None, // TODO
            },
        )
        .await
        .unwrap();

        // Create bucket in actual storages
        let mut tasks = tokio::task::JoinSet::new();
        let locators = create_bucket_resp.locators;

        for locator in locators {
            let client: Arc<Box<dyn ObjectStoreClient>> =
                self.store_clients.get(&locator.tag).unwrap().clone();

            let bucket_name = locator.bucket.clone();
            let dir_conf = self.dir_conf.clone();

            // TODO: fix
            // if buckt name starts with skystore-, then it's a skybucket already created during initialization
            // so we don't need to create it again
            if bucket_name.starts_with("skystore-") {
                let system_time: SystemTime = Utc::now().into();
                let timestamp: s3s::dto::Timestamp = system_time.into();

                apis::complete_create_bucket(
                    &dir_conf,
                    models::CreateBucketIsCompleted {
                        id: locator.id,
                        creation_date: timestamp_to_string(timestamp), // TODO: fix this, get metadata from actual storage
                    },
                )
                .await
                .unwrap();
                continue;
            }

            tasks.spawn(async move {
                let create_bucket_req = new_create_bucket_request(bucket_name.clone(), None);
                client
                    .create_bucket(S3Request::new(create_bucket_req))
                    .await
                    .unwrap();

                let system_time: SystemTime = Utc::now().into();
                let timestamp: s3s::dto::Timestamp = system_time.into();

                apis::complete_create_bucket(
                    &dir_conf,
                    models::CreateBucketIsCompleted {
                        id: locator.id,
                        creation_date: timestamp_to_string(timestamp), // TODO: fix this, get metadata from actual storage
                    },
                )
                .await
                .unwrap();
            });
        }

        // TODO: handle cleanup if create bucket fails
        while let Some(result) = tasks.join_next().await {
            if let Err(err) = result {
                return Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    format!("Error creating bucket: {}", err),
                ));
            }
        }

        Ok(S3Response::new(CreateBucketOutput::default()))
    }

    #[tracing::instrument(level = "info")]
    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        // Send start delete bucket request
        let delete_bucket_resp = apis::start_delete_bucket(
            &self.dir_conf,
            models::DeleteBucketRequest {
                bucket: req.input.bucket.clone(),
            },
        )
        .await
        .unwrap();

        // Delete bucket in actual storages
        let mut tasks = tokio::task::JoinSet::new();
        let locators = delete_bucket_resp.locators;

        for locator in locators {
            let client: Arc<Box<dyn ObjectStoreClient>> =
                self.store_clients.get(&locator.tag).unwrap().clone();

            let bucket_name = req.input.bucket.clone();
            let dir_conf = self.dir_conf.clone();

            tasks.spawn(async move {
                let delete_bucket_req = new_delete_bucket_request(bucket_name.clone());
                client
                    .delete_bucket(S3Request::new(delete_bucket_req))
                    .await
                    .unwrap();

                apis::complete_delete_bucket(
                    &dir_conf,
                    models::DeleteBucketIsCompleted { id: locator.id },
                )
                .await
                .unwrap();
            });
        }

        while let Some(result) = tasks.join_next().await {
            if let Err(err) = result {
                return Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    format!("Error deleting bucket: {}", err),
                ));
            }
        }

        Ok(S3Response::new(DeleteBucketOutput::default()))
    }

    #[tracing::instrument(level = "info")]
    async fn list_buckets(
        &self,
        _req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let resp = apis::list_buckets(&self.dir_conf).await;

        match resp {
            Ok(resp) => {
                let mut buckets: Vec<Bucket> = Vec::new();

                for bucket in resp {
                    buckets.push(Bucket {
                        creation_date: Some(string_to_timestamp(&bucket.creation_date)),
                        name: Some(bucket.bucket),
                    });
                }

                Ok(S3Response::new(ListBucketsOutput {
                    buckets: Some(buckets),
                    owner: None,
                }))
            }
            Err(err) => {
                error!("list_buckets failed: {:?}", err);
                Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    "list_buckets failed",
                ))
            }
        }
    }

    #[tracing::instrument(level = "info")]
    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let resp = apis::list_objects(
            &self.dir_conf,
            models::ListObjectRequest {
                bucket: req.input.bucket.clone(),
                prefix: req.input.prefix.clone(),
            },
        )
        .await;

        match resp {
            Ok(resp) => {
                let mut objects: Vec<Object> = Vec::new();

                for obj in resp {
                    objects.push(Object {
                        key: Some(obj.key),
                        size: obj.size as i64,
                        last_modified: Some(string_to_timestamp(&obj.last_modified)),
                        ..Default::default()
                    })
                }

                Ok(S3Response::new(ListObjectsV2Output {
                    contents: Some(objects),
                    ..Default::default()
                }))
            }
            Err(err) => {
                error!("list_objects_v2 failed: {:?}", err);
                Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    "list_objects_v2 failed",
                ))
            }
        }
    }

    #[tracing::instrument(level = "info")]
    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let locator = self
            .locate_object(req.input.bucket.clone(), req.input.key.clone())
            .await?;

        match locator {
            Some(locator) => {
                let client: Arc<Box<dyn ObjectStoreClient>> =
                    self.store_clients.get(&locator.tag).unwrap().clone();

                match client
                    .head_object(S3Request::new(new_head_object_request(
                        locator.bucket.clone(),
                        locator.key.clone(),
                    )))
                    .await
                {
                    Ok(resp) => Ok(resp),
                    Err(err) => {
                        error!("head_object failed: {:?}", err);
                        Err(s3s::S3Error::with_message(
                            s3s::S3ErrorCode::InternalError,
                            "head_object failed",
                        ))
                    }
                }
            }
            None => Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::NoSuchKey,
                "No such key",
            )),
        }
    }

    #[tracing::instrument(level = "info")]
    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let locator = self
            .locate_object(req.input.bucket.clone(), req.input.key.clone())
            .await?;

        match locator {
            Some(location) => {
                self.store_clients
                    .get(&location.tag)
                    .unwrap()
                    .get_object(req.map_input(|mut input: GetObjectInput| {
                        input.bucket = location.bucket;
                        input.key = location.key;
                        input
                    }))
                    .await
            }
            None => Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::NoSuchKey,
                "Object not found",
            )),
        }
    }

    #[tracing::instrument(level = "info")]
    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        // Idempotent PUT
        let locator = self
            .locate_object(req.input.bucket.clone(), req.input.key.clone())
            .await?;
        if let Some(locator) = locator {
            return Ok(S3Response::new(PutObjectOutput {
                e_tag: locator.etag,
                ..Default::default()
            }));
        }

        let start_upload_resp = apis::start_upload(
            &self.dir_conf,
            models::StartUploadRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                client_from_region: self.client_from_region.clone(),
                is_multipart: false,
                copy_src_bucket: None,
                copy_src_key: None,
            },
        )
        .await
        .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        let locators = start_upload_resp.locators;
        let request_template = clone_put_object_request(&req.input, None);
        let input_blobs = split_streaming_blob(req.input.body.unwrap(), locators.len());

        locators
            .into_iter()
            .zip(input_blobs.into_iter())
            .for_each(|(locator, input_blob)| {
                let conf = self.dir_conf.clone();
                let client: Arc<Box<dyn ObjectStoreClient>> =
                    self.store_clients.get(&locator.tag).unwrap().clone();
                let req = S3Request::new(clone_put_object_request(
                    &request_template,
                    Some(input_blob),
                ))
                .map_input(|mut input| {
                    input.bucket = locator.bucket.clone();
                    input.key = locator.key.clone();
                    input
                });

                tasks.spawn(async move {
                    let put_resp = client.put_object(req).await.unwrap();
                    let e_tag = put_resp.output.e_tag.unwrap();

                    // Retrieve the object metatada through HEAD request.
                    // So we get the proper size, etag, and last_modified from the object store pov.
                    let head_resp = client
                        .head_object(S3Request::new(new_head_object_request(
                            locator.bucket.clone(),
                            locator.key.clone(),
                        )))
                        .await
                        .unwrap();

                    apis::complete_upload(
                        &conf,
                        models::PatchUploadIsCompleted {
                            id: locator.id,
                            size: head_resp.output.content_length as u64,
                            etag: e_tag.clone(),
                            last_modified: timestamp_to_string(
                                head_resp.output.last_modified.unwrap(),
                            ),
                        },
                    )
                    .await
                    .unwrap();

                    e_tag
                });
            });

        let mut e_tags = Vec::new();
        while let Some(Ok(e_tag)) = tasks.join_next().await {
            e_tags.push(e_tag);
        }

        Ok(S3Response::new(PutObjectOutput {
            e_tag: e_tags.pop(),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        // Send start delete objects request
        let keys: Vec<_> = req
            .input
            .delete
            .objects
            .iter()
            .map(|obj| obj.key.clone())
            .collect();
        let delete_objects_resp = apis::start_delete_objects(
            &self.dir_conf,
            models::DeleteObjectsRequest {
                bucket: req.input.bucket.clone(),
                keys,
            },
        )
        .await
        .unwrap();

        // Delete objects in actual storages
        let mut tasks = tokio::task::JoinSet::new();
        for (key, locators) in delete_objects_resp.locators {
            for locator in locators {
                let client: Arc<Box<dyn ObjectStoreClient>> =
                    self.store_clients.get(&locator.tag).unwrap().clone();
                let bucket_name = locator.bucket.clone();
                let key_clone = key.clone(); // Clone the key here

                tasks.spawn(async move {
                    let delete_object_response = client
                        .delete_object(S3Request::new(new_delete_object_request(
                            bucket_name.clone(),
                            locator.key,
                        )))
                        .await;

                    match delete_object_response {
                        Ok(_resp) => Ok((key_clone.clone(), locator.id)),
                        Err(_) => Err((
                            key_clone.clone(),
                            format!("Failed to delete object with key: {}", key_clone),
                        )),
                    }
                });
            }
        }

        // Collect results and complete delete objects
        let mut key_to_results: HashMap<String, Vec<Result<i32, String>>> = HashMap::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(res) => match res {
                    Ok((key, id)) => {
                        key_to_results.entry(key).or_default().push(Ok(id));
                    }
                    Err((key, err)) => {
                        key_to_results.entry(key).or_default().push(Err(err));
                    }
                },
                // TODO: how to handle this?
                Err(err) => {
                    return Err(s3s::S3Error::with_message(
                        s3s::S3ErrorCode::InternalError,
                        format!("Error deleting objects: {}", err),
                    ));
                }
            }
        }

        let mut deleted_objects = DeletedObjects::default();
        let mut errors = Errors::default();

        for (key, results) in key_to_results {
            let (successes, fails): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);

            // complete delete objects
            let completed_ids: Vec<_> = successes.into_iter().filter_map(Result::ok).collect();
            if !completed_ids.is_empty() {
                apis::complete_delete_objects(
                    &self.dir_conf,
                    models::DeleteObjectsIsCompleted { ids: completed_ids },
                )
                .await
                .unwrap();
            }

            if fails.is_empty() {
                deleted_objects.push(DeletedObject {
                    key: Some(key),
                    delete_marker: false, // TODO: add versioning support
                    ..Default::default()
                });
            } else {
                errors.push(Error {
                    key: Some(key),
                    code: Some("InternalError".to_string()),
                    message: Some(fails.into_iter().map(Result::unwrap_err).collect()),
                    ..Default::default()
                });
            }
        }

        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: Some(deleted_objects),
            errors: if errors.is_empty() {
                None
            } else {
                Some(errors)
            },
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let delete = Delete {
            objects: vec![ObjectIdentifier {
                key: req.input.key.clone(),
                version_id: req.input.version_id.clone(),
            }],
            ..Default::default()
        };

        let delete_object_input = new_delete_objects_request(req.input.bucket.clone(), delete);
        let delete_object_req = S3Request::new(delete_object_input);

        match self.delete_objects(delete_object_req).await {
            Ok(delete_objects_resp) => {
                if delete_objects_resp
                    .output
                    .deleted
                    .and_then(|objs| {
                        objs.into_iter()
                            .find(|obj| obj.key.as_ref() == Some(&req.input.key))
                    })
                    .is_some()
                {
                    Ok(S3Response::new(DeleteObjectOutput {
                        delete_marker: false, // TODO: add versioning support
                        ..Default::default()
                    }))
                } else {
                    Err(s3s::S3Error::with_message(
                        s3s::S3ErrorCode::InternalError,
                        "Object not deleted successfully".to_string(),
                    ))
                }
            }
            Err(e) => Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                format!("Error deleting object: {}", e),
            )),
        }
    }

    #[tracing::instrument(level = "info")]
    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        //TODO for future: people commonly use copy_object to modify the metadata, instead of creatig a new today.

        let [src_bucket, src_key] = match &req.input.copy_source {
            CopySource::Bucket { bucket, key, .. } => [bucket, key],
            CopySource::AccessPoint { .. } => panic!("AccessPoint not supported"),
        };
        let [dst_bucket, dst_key] = [&req.input.bucket, &req.input.key];

        let src_locator = self
            .locate_object(src_bucket.to_string(), src_key.to_string())
            .await?;
        let dst_locator = self
            .locate_object(dst_bucket.to_string(), dst_key.to_string())
            .await?;

        if src_locator.is_none() {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::NoSuchKey,
                "Source object not found",
            ));
        };

        if let Some(dst_locator) = dst_locator {
            return Ok(S3Response::new(CopyObjectOutput {
                copy_object_result: Some(CopyObjectResult {
                    e_tag: dst_locator.etag,
                    last_modified: Some(string_to_timestamp(
                        dst_locator.last_modified.as_ref().unwrap(),
                    )),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        };

        let upload_resp = apis::start_upload(
            &self.dir_conf,
            models::StartUploadRequest {
                bucket: dst_bucket.to_string(),
                key: dst_key.to_string(),
                client_from_region: self.client_from_region.clone(),
                is_multipart: false,
                copy_src_bucket: Some(src_bucket.to_string()),
                copy_src_key: Some(src_key.to_string()),
            },
        )
        .await
        .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        upload_resp
            .locators
            .into_iter()
            .zip(
                upload_resp
                    .copy_src_buckets
                    .into_iter()
                    .zip(upload_resp.copy_src_keys.into_iter()),
            )
            .for_each(|(locator, (src_bucket, src_key))| {
                let conf = self.dir_conf.clone();
                let client = self.store_clients.get(&locator.tag).unwrap().clone();

                tasks.spawn(async move {
                    let copy_resp = client
                        .copy_object(S3Request::new(new_copy_object_request(
                            src_bucket.clone(),
                            src_key.clone(),
                            locator.bucket.clone(),
                            locator.key.clone(),
                        )))
                        .await
                        .unwrap();

                    let etag = copy_resp.output.copy_object_result.unwrap().e_tag.unwrap();

                    let head_output = client
                        .head_object(S3Request::new(new_head_object_request(
                            locator.bucket.clone(),
                            locator.key.clone(),
                        )))
                        .await
                        .unwrap();

                    apis::complete_upload(
                        &conf,
                        models::PatchUploadIsCompleted {
                            id: locator.id,
                            size: head_output.output.content_length as u64,
                            etag: etag.clone(),
                            last_modified: timestamp_to_string(
                                head_output.output.last_modified.unwrap(),
                            ),
                        },
                    )
                    .await
                    .unwrap();

                    etag
                });
            });

        let mut e_tags = Vec::new();
        while let Some(Ok(e_tag)) = tasks.join_next().await {
            e_tags.push(e_tag);
        }

        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: e_tags.pop(),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let locator = self
            .locate_object(req.input.bucket.clone(), req.input.key.clone())
            .await?;

        if locator.is_some() {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "Object already exists. TODO for multipart upload overwite.",
            ));
        };

        let upload_resp = apis::start_upload(
            &self.dir_conf,
            models::StartUploadRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                client_from_region: self.client_from_region.clone(),
                is_multipart: true,
                copy_src_bucket: None,
                copy_src_key: None,
            },
        )
        .await
        .unwrap();

        assert!(upload_resp.multipart_upload_id.is_some());

        // it seems we don't need to do this in parallel.
        // this is just a metadata change afterall
        for locator in upload_resp.locators.iter() {
            let client = self.store_clients.get(&locator.tag).unwrap().clone();

            let resp = client
                .create_multipart_upload(S3Request::new(new_create_multipart_upload_request(
                    locator.bucket.clone(),
                    locator.key.clone(),
                )))
                .await
                .unwrap();

            let physical_multipart_id = resp.output.upload_id.unwrap();

            apis::set_multipart_id(
                &self.dir_conf,
                models::PatchUploadMultipartUploadId {
                    id: locator.id,
                    multipart_upload_id: physical_multipart_id,
                },
            )
            .await
            .unwrap();
        }

        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(req.input.bucket.clone()),
            key: Some(req.input.key.clone()),
            upload_id: upload_resp.multipart_upload_id,
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let resp = apis::list_multipart_uploads(
            &self.dir_conf,
            models::ListObjectRequest {
                bucket: req.input.bucket.clone(),
                prefix: req.input.prefix.clone(),
            },
        )
        .await
        .unwrap();

        Ok(S3Response::new(ListMultipartUploadsOutput {
            uploads: Some(
                resp.into_iter()
                    .map(|u| MultipartUpload {
                        key: Some(u.key),
                        upload_id: Some(u.upload_id),
                        ..Default::default()
                    })
                    .collect(),
            ),
            bucket: Some(req.input.bucket.clone()),
            prefix: req.input.prefix.clone(),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let resp = apis::list_parts(
            &self.dir_conf,
            models::ListPartsRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                upload_id: req.input.upload_id.clone(),
                part_number: None,
            },
        )
        .await
        .unwrap();

        Ok(S3Response::new(ListPartsOutput {
            parts: Some(
                resp.into_iter()
                    .map(|p| Part {
                        part_number: p.part_number,
                        size: p.size as i64,
                        e_tag: Some(p.etag),
                        ..Default::default()
                    })
                    .collect(),
            ),
            bucket: Some(req.input.bucket.clone()),
            key: Some(req.input.key.clone()),
            upload_id: Some(req.input.upload_id.clone()),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let resp = apis::continue_upload(
            &self.dir_conf,
            models::ContinueUploadRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                multipart_upload_id: req.input.upload_id.clone(),
                client_from_region: self.client_from_region.clone(),
                do_list_parts: Some(false),
                copy_src_bucket: None,
                copy_src_key: None,
            },
        )
        .await
        .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        let request_template = clone_upload_part_request(&req.input, None);
        let content_length = req
            .input
            .body
            .as_ref()
            .unwrap()
            .remaining_length()
            .exact()
            .unwrap();
        let body_clones = split_streaming_blob(req.input.body.unwrap(), resp.len());
        resp.into_iter()
            .zip(body_clones.into_iter())
            .for_each(|(locator, body)| {
                let conf = self.dir_conf.clone();
                let client = self.store_clients.get(&locator.tag).unwrap().clone();
                let req = S3Request::new(clone_upload_part_request(&request_template, Some(body)))
                    .map_input(|mut i| {
                        i.bucket = locator.bucket.clone();
                        i.key = locator.key.clone();
                        i.upload_id = locator.multipart_upload_id.clone();
                        i
                    });
                let part_number = req.input.part_number;

                tasks.spawn(async move {
                    let resp: UploadPartOutput = client.upload_part(req).await.unwrap().output;

                    let etag = resp.e_tag.unwrap();

                    apis::append_part(
                        &conf,
                        models::PatchUploadMultipartUploadPart {
                            id: locator.id,
                            part_number,
                            etag: etag.clone(),
                            size: content_length as u64,
                        },
                    )
                    .await
                    .unwrap();
                });
            });

        while (tasks.join_next().await).is_some() {}

        let parts = apis::list_parts(
            &self.dir_conf,
            models::ListPartsRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                upload_id: req.input.upload_id.clone(),
                part_number: Some(req.input.part_number),
            },
        )
        .await
        .unwrap();
        assert!(parts.len() == 1);

        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(parts[0].etag.clone()),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let [src_bucket, src_key] = match &req.input.copy_source {
            CopySource::Bucket { bucket, key, .. } => [bucket.to_string(), key.to_string()],
            CopySource::AccessPoint { .. } => panic!("AccessPoint not supported"),
        };

        let src_locator = self
            .locate_object(src_bucket.clone(), src_key.clone())
            .await
            .unwrap();
        if src_locator.is_none() {
            return Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::NoSuchKey,
                "Source object not found",
            ));
        };
        let content_length = src_locator.as_ref().unwrap().size.unwrap();

        let locators = apis::continue_upload(
            &self.dir_conf,
            models::ContinueUploadRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                multipart_upload_id: req.input.upload_id.clone(),
                client_from_region: self.client_from_region.clone(),
                do_list_parts: Some(false),
                copy_src_bucket: Some(src_bucket.clone()),
                copy_src_key: Some(src_key.clone()),
            },
        )
        .await
        .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        locators.into_iter().for_each(|locator| {
            let conf = self.dir_conf.clone();
            let client = self.store_clients.get(&locator.tag).unwrap().clone();
            let part_number = req.input.part_number;
            tasks.spawn(async move {
                let req = new_upload_part_copy_request(
                    locator.bucket.clone(),
                    locator.key.clone(),
                    locator.multipart_upload_id.clone(),
                    part_number,
                    locator.copy_src_bucket.clone().unwrap(),
                    locator.copy_src_key.clone().unwrap(),
                );
                let copy_resp = client.upload_part_copy(S3Request::new(req)).await.unwrap();

                apis::append_part(
                    &conf,
                    models::PatchUploadMultipartUploadPart {
                        id: locator.id,
                        part_number,
                        etag: copy_resp.output.copy_part_result.unwrap().e_tag.unwrap(),
                        size: content_length, // we actually can't head object this :(, this is a part.
                    },
                )
                .await
                .unwrap();
            });
        });

        while (tasks.join_next().await).is_some() {}

        let parts = apis::list_parts(
            &self.dir_conf,
            models::ListPartsRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                upload_id: req.input.upload_id.clone(),
                part_number: Some(req.input.part_number),
            },
        )
        .await
        .unwrap();
        assert!(parts.len() == 1, "parts: {parts:?}");

        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(parts[0].etag.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    #[tracing::instrument(level = "info")]
    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let locators = apis::continue_upload(
            &self.dir_conf,
            models::ContinueUploadRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
                multipart_upload_id: req.input.upload_id.clone(),
                client_from_region: self.client_from_region.clone(),
                do_list_parts: Some(true),
                copy_src_bucket: None,
                copy_src_key: None,
            },
        )
        .await
        .unwrap();

        let logical_parts_set = req
            .input
            .multipart_upload
            .unwrap()
            .parts
            .as_ref()
            .unwrap()
            .iter()
            .map(|p| (p.part_number))
            .collect::<HashSet<_>>();

        for locator in locators {
            let client = self.store_clients.get(&locator.tag).unwrap().clone();

            let mut physical_parts_list: Vec<_> = locator
                .parts
                .unwrap()
                .into_iter()
                .map(|p| {
                    let mut part = CompletedPart::default();
                    assert!(logical_parts_set.contains(&p.part_number));
                    part.part_number = p.part_number;
                    part.e_tag = Some(p.etag);
                    part
                })
                .collect();

            physical_parts_list.sort_by_key(|p| p.part_number);

            assert!(physical_parts_list.len() == logical_parts_set.len());

            let resp = client
                .complete_multipart_upload(S3Request::new(new_complete_multipart_request(
                    locator.bucket.clone(),
                    locator.key.clone(),
                    locator.multipart_upload_id.clone(),
                    CompletedMultipartUpload {
                        parts: Some(physical_parts_list),
                    },
                )))
                .await
                .unwrap();

            let head_resp = client
                .head_object(S3Request::new(new_head_object_request(
                    locator.bucket.clone(),
                    locator.key.clone(),
                )))
                .await
                .unwrap();

            apis::complete_upload(
                &self.dir_conf,
                models::PatchUploadIsCompleted {
                    id: locator.id,
                    etag: resp.output.e_tag.unwrap(),
                    size: head_resp.output.content_length as u64,
                    last_modified: timestamp_to_string(head_resp.output.last_modified.unwrap()),
                },
            )
            .await
            .unwrap();
        }

        let head_resp = apis::head_object(
            &self.dir_conf,
            models::HeadObjectRequest {
                bucket: req.input.bucket.clone(),
                key: req.input.key.clone(),
            },
        )
        .await
        .unwrap();

        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(req.input.bucket.clone()),
            key: Some(req.input.key.clone()),
            e_tag: Some(head_resp.etag),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_static::lazy_static;
    use serial_test::serial;

    lazy_static! {
        static ref REGIONS: Vec<String> = vec![
            "aws:us-west-1".to_string(),
            "aws:us-east-2".to_string(),
            "gcp:us-west1".to_string(),
            "aws:eu-central-1".to_string(),
        ];
        static ref CLIENT_FROM_REGION: String = "aws:us-west-1".to_string();
    }

    fn generate_unique_bucket_name() -> String {
        let timestamp = chrono::Utc::now().timestamp_nanos();
        format!("my-bucket-{}", timestamp)
    }

    #[tokio::test]
    #[serial]
    async fn test_constructor() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;
        assert!(!proxy.store_clients.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn test_list_objects() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        // create a bucket
        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        let request = new_list_objects_v2_input(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        let resp = proxy.list_objects_v2(req).await.unwrap().output;
        assert!(resp.contents.is_some());
    }

    #[tokio::test]
    #[serial]
    async fn test_put_then_get() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        // create a bucket
        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        {
            let mut request = new_put_object_request(bucket_name.to_string(), "my-key".to_string());
            let body = "abcdefg".to_string().into_bytes();
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            let resp = proxy.put_object(req).await.unwrap().output;
            assert!(resp.e_tag.is_some());
        }

        {
            let request =
                new_list_objects_v2_input(bucket_name.to_string(), Some("my-key".to_string()));
            let req = S3Request::new(request);
            let resp = proxy.list_objects_v2(req).await.unwrap().output;
            assert!(resp.contents.is_some());
            assert!(resp.contents.unwrap().len() == 1);
        }

        {
            let request = new_get_object_request(bucket_name.to_string(), "my-key".to_string());
            let req = S3Request::new(request);
            let resp = proxy.get_object(req).await.unwrap().output;
            assert!(resp.body.is_some());

            let resp_body = resp.body.unwrap();

            use tokio_stream::StreamExt;

            let result_bytes = resp_body
                .map(|chunk| chunk.unwrap())
                .collect::<Vec<_>>()
                .await;

            let body = result_bytes.concat();
            assert!(body == "abcdefg".to_string().into_bytes());
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_delete_objects() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        for i in 0..3 {
            let mut request =
                new_put_object_request(bucket_name.to_string(), format!("my-key-{}", i));
            let body = format!("data-{}", i).into_bytes();
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            proxy.put_object(req).await.unwrap().output;
        }

        let delete = Delete {
            objects: vec![
                ObjectIdentifier {
                    key: "my-key-0".to_string(),
                    version_id: None,
                },
                ObjectIdentifier {
                    key: "my-key-1".to_string(),
                    version_id: None,
                },
            ],
            ..Default::default()
        };

        let delete_objects_input = new_delete_objects_request(bucket_name.to_string(), delete);
        let delete_objects_req = S3Request::new(delete_objects_input);
        proxy
            .delete_objects(delete_objects_req)
            .await
            .unwrap()
            .output;

        // Verify objects are deleted
        let list_request = new_list_objects_v2_input(bucket_name.to_string(), None);
        let list_resp = proxy
            .list_objects_v2(S3Request::new(list_request))
            .await
            .unwrap()
            .output;
        assert!(list_resp.contents.unwrap().len() == 1);
    }

    #[tokio::test]
    #[serial]
    async fn test_delete_object() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        {
            let mut request =
                new_put_object_request(bucket_name.to_string(), "my-single-key".to_string());
            let body = "single-data".to_string().into_bytes();
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            proxy.put_object(req).await.unwrap().output;
        }

        let delete_object_input =
            new_delete_object_request(bucket_name.to_string(), "my-single-key".to_string());
        let delete_object_req = S3Request::new(delete_object_input);
        proxy.delete_object(delete_object_req).await.unwrap().output;

        // Verify a single object was deleted
        let list_request = new_list_objects_v2_input(bucket_name.to_string(), None);
        let list_resp = proxy
            .list_objects_v2(S3Request::new(list_request))
            .await
            .unwrap()
            .output;
        assert!(list_resp.contents.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn test_copy_object() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        // create a bucket
        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        // put an object to my-bucket/my-copy-key
        {
            let mut request =
                new_put_object_request(bucket_name.to_string(), "my-copy-key".to_string());
            let body = "abcdefg".to_string().into_bytes();
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            let resp = proxy.put_object(req).await.unwrap().output;
            assert!(resp.e_tag.is_some());
        }

        // copy the object
        {
            let request = new_copy_object_request(
                bucket_name.to_string(),
                "my-copy-key".to_string(),
                bucket_name.to_string(),
                "my-copy-key-copy".to_string(),
            );
            let req = S3Request::new(request);
            let resp = proxy.copy_object(req).await.unwrap().output;
            assert!(resp.copy_object_result.is_some());
        }

        // get the object at my-copy-key-copy
        {
            let request =
                new_get_object_request(bucket_name.to_string(), "my-copy-key-copy".to_string());
            let req = S3Request::new(request);
            let resp = proxy.get_object(req).await.unwrap().output;
            assert!(resp.body.is_some());

            let resp_body = resp.body.unwrap();

            use tokio_stream::StreamExt;

            let result_bytes = resp_body
                .map(|chunk| chunk.unwrap())
                .collect::<Vec<_>>()
                .await;

            let body = result_bytes.concat();
            assert!(body == "abcdefg".to_string().into_bytes());
        }
    }

    #[tokio::test]
    #[serial]
    #[ignore = "UploadPartCopy is not implemented in the emulator."]
    async fn test_multipart_flow() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        // create a bucket
        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        // AWS's The minimal multipart upload size is 5Mb
        // which is pretty sad but we have to test it against real service here.
        let part_size = 10 * 1024 * 1024;

        // initiate multipart upload
        let upload_id = {
            let request = new_create_multipart_upload_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
            );
            let req = S3Request::new(request);
            let resp = proxy.create_multipart_upload(req).await.unwrap().output;
            assert!(resp.upload_id.is_some());
            resp.upload_id.unwrap()
        };

        // test list multipart upload contains upload_id
        {
            let request = new_list_multipart_uploads_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
            );
            let req = S3Request::new(request);
            let resp = proxy.list_multipart_uploads(req).await.unwrap().output;
            assert!(resp.uploads.is_some());
            let uploads = resp.uploads.unwrap();
            assert!(!uploads.is_empty());
            let found_upload = uploads
                .iter()
                .find(|upload: &&MultipartUpload| upload.upload_id == Some(upload_id.clone()));
            assert!(found_upload.is_some());
        }

        // upload part 1
        let etag1 = {
            let mut request = new_upload_part_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
                upload_id.clone(),
                1,
            );
            let body: Vec<u8> = vec![0; part_size];
            // let body: Vec<u8> = vec![0; 6];
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            let resp = proxy.upload_part(req).await.unwrap().output;

            resp.e_tag.unwrap()
        };

        // list parts
        {
            let request = new_list_parts_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
                upload_id.clone(),
            );
            let req = S3Request::new(request);
            let resp = proxy.list_parts(req).await.unwrap().output;
            assert!(resp.parts.is_some());
            let parts = resp.parts.unwrap();
            assert!(parts.len() == 1);
            assert!(parts[0].part_number == 1);
        }

        // test upload part copy
        let etag2 = {
            // start by uploading a simple object
            let mut request =
                new_put_object_request(bucket_name.to_string(), "my-copy-src-key".to_string());
            let body: Vec<u8> = vec![0; part_size];
            request.body = Some(s3s::Body::from(body).into());
            let req = S3Request::new(request);
            let resp = proxy.put_object(req).await.unwrap().output;
            assert!(resp.e_tag.is_some());

            // now issue a copy part request
            let request = new_upload_part_copy_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
                upload_id.clone(),
                2,
                bucket_name.to_string(),
                "my-copy-src-key".to_string(),
            );

            let req = S3Request::new(request);
            let resp = proxy.upload_part_copy(req).await.unwrap().output;
            assert!(resp.copy_part_result.is_some());
            resp.copy_part_result.unwrap().e_tag.unwrap()
        };

        // complete the upload
        {
            let request = new_complete_multipart_request(
                bucket_name.to_string(),
                "my-multipart-key".to_string(),
                upload_id.clone(),
                CompletedMultipartUpload {
                    parts: Some(vec![
                        CompletedPart {
                            e_tag: Some(etag1),
                            part_number: 1,
                            checksum_crc32: None,
                            checksum_crc32c: None,
                            checksum_sha1: None,
                            checksum_sha256: None,
                        },
                        CompletedPart {
                            e_tag: Some(etag2),
                            part_number: 2,
                            checksum_crc32: None,
                            checksum_crc32c: None,
                            checksum_sha1: None,
                            checksum_sha256: None,
                        },
                    ]),
                },
            );
            let req = S3Request::new(request);
            let resp = proxy.complete_multipart_upload(req).await.unwrap().output;
            assert!(resp.e_tag.is_some());

            // We should able to get the content of the object
            let request =
                new_get_object_request(bucket_name.to_string(), "my-multipart-key".to_string());
            let req = S3Request::new(request);
            let resp = proxy.get_object(req).await.unwrap().output;
            assert!(resp.body.is_some());

            let resp_body = resp.body.unwrap();

            use tokio_stream::StreamExt;

            let result_bytes = resp_body
                .map(|chunk| chunk.unwrap())
                .collect::<Vec<_>>()
                .await;

            let body = result_bytes.concat();
            assert!(body.len() == part_size * 2);
            // assert!(body.len() == 6 * 2);
            assert!(body[0] == 0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_multipart_many_parts() {
        let proxy = SkyProxy::new(REGIONS.clone(), CLIENT_FROM_REGION.clone()).await;

        // create a bucket
        let bucket_name = generate_unique_bucket_name();
        let request = new_create_bucket_request(bucket_name.to_string(), None);
        let req = S3Request::new(request);
        proxy.create_bucket(req).await.unwrap().output;

        // initiate multipart upload
        let upload_id = {
            let request = new_create_multipart_upload_request(
                bucket_name.to_string(),
                "my-multipart-many-parts-key".to_string(),
            );
            let req = S3Request::new(request);
            let resp = proxy.create_multipart_upload(req).await.unwrap().output;
            assert!(resp.upload_id.is_some());
            resp.upload_id.unwrap()
        };

        // upload 100 parts
        let mut etags = Vec::new();
        for i in 1..=40 {
            let mut request = new_upload_part_request(
                bucket_name.to_string(),
                "my-multipart-many-parts-key".to_string(),
                upload_id.clone(),
                i,
            );
            let body: Vec<u8> = vec![0; 5 * 1024 * 1024];
            request.body = Some(s3s::Body::from(body).into());

            let req = S3Request::new(request);
            let resp = proxy.upload_part(req).await.unwrap().output;

            etags.push(resp.e_tag.unwrap());
        }

        // complete the upload
        {
            let request = new_complete_multipart_request(
                bucket_name.to_string(),
                "my-multipart-many-parts-key".to_string(),
                upload_id.clone(),
                CompletedMultipartUpload {
                    parts: Some(
                        etags
                            .iter()
                            .enumerate()
                            .map(|(i, etag)| CompletedPart {
                                e_tag: Some(etag.clone()),
                                part_number: i as i32 + 1,
                                checksum_crc32: None,
                                checksum_crc32c: None,
                                checksum_sha1: None,
                                checksum_sha256: None,
                            })
                            .collect(),
                    ),
                },
            );
            let req = S3Request::new(request);
            let resp = proxy.complete_multipart_upload(req).await.unwrap().output;
            assert!(resp.e_tag.is_some());

            // We should able to get the content of the object
            let request = new_get_object_request(
                bucket_name.to_string(),
                "my-multipart-many-parts-key".to_string(),
            );
            let req = S3Request::new(request);
            let resp = proxy.get_object(req).await.unwrap().output;
            assert!(resp.body.is_some());

            let resp_body = resp.body.unwrap();

            use tokio_stream::StreamExt;

            let result_bytes = resp_body
                .map(|chunk| chunk.unwrap())
                .collect::<Vec<_>>()
                .await;

            let body = result_bytes.concat();
            assert!(body.len() == 40 * 5 * 1024 * 1024);
        }
    }
}
