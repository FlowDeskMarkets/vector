use futures_util::{
    stream::{self, BoxStream},
    StreamExt,
};
use vector_lib::event::Event;
use vector_lib::sink::StreamSink;
use vector_lib::stream::BatcherSettings;

use super::request_builder::BigqueryRequestBuilder;
use super::service::BigqueryService;
use crate::sinks::prelude::SinkRequestBuildError;
use crate::sinks::util::builder::SinkBuilderExt;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use super::proto::third_party::google::cloud::bigquery::storage::v1 as proto;
use super::service::BigqueryRequestType;

pub struct BigquerySink {
    pub service: BigqueryService,
    pub batcher_settings: BatcherSettings,
    pub request_builder: BigqueryRequestBuilder,
    pub use_pending_streams: bool,
    pub parent_path: String,
}

impl BigquerySink {
    // Helper method to create a pending write stream
    async fn create_pending_stream(&self) -> Result<String, crate::Error> {
        // Create a request to create a pending write stream
        let create_request = proto::CreateWriteStreamRequest {
            parent: self.parent_path.clone(),
            write_stream: Some(proto::WriteStream {
                name: String::new(),
                type_: proto::WriteStream::Type::Pending.into(),
                create_time: None,
                commit_time: None,
                table_schema: None,
            }),
        };

        // The uncompressed size is approximate here
        let uncompressed_size = create_request.encoded_len();
        
        // Create a request with empty metadata/finalizers
        let request = super::service::BigqueryRequest {
            request_type: BigqueryRequestType::CreateWriteStream(create_request),
            metadata: Default::default(),
            finalizers: Default::default(),
            uncompressed_size,
        };

        // Send the request to create the stream
        let mut service = self.service.clone();
        match service.call(request).await {
            Ok(response) => {
                if let super::service::BigqueryResponseBody::CreateWriteStream(stream) = response.body {
                    debug!(message = "Created pending write stream", stream_name = %stream.name);
                    Ok(stream.name)
                } else {
                    Err("Unexpected response type from CreateWriteStream".into())
                }
            },
            Err(err) => {
                error!(message = "Failed to create pending write stream", error = ?err);
                Err(format!("Failed to create pending write stream: {:?}", err).into())
            }
        }
    }

    // Helper method to finalize a write stream
    async fn finalize_stream(&self, stream_name: String) -> Result<u64, crate::Error> {
        // Create a request to finalize the stream
        let finalize_request = proto::FinalizeWriteStreamRequest {
            name: stream_name,
        };

        // The uncompressed size is approximate here
        let uncompressed_size = finalize_request.encoded_len();
        
        // Create a request with empty metadata/finalizers
        let request = super::service::BigqueryRequest {
            request_type: BigqueryRequestType::FinalizeWriteStream(finalize_request),
            metadata: Default::default(),
            finalizers: Default::default(),
            uncompressed_size,
        };

        // Send the request to finalize the stream
        let mut service = self.service.clone();
        match service.call(request).await {
            Ok(response) => {
                if let super::service::BigqueryResponseBody::FinalizeWriteStream(result) = response.body {
                    debug!(message = "Finalized write stream", row_count = %result.row_count);
                    Ok(result.row_count)
                } else {
                    Err("Unexpected response type from FinalizeWriteStream".into())
                }
            },
            Err(err) => {
                error!(message = "Failed to finalize write stream", error = ?err);
                Err(format!("Failed to finalize write stream: {:?}", err).into())
            }
        }
    }

    // Helper method to batch commit write streams
    async fn batch_commit_streams(&self, stream_names: Vec<String>) -> Result<(), crate::Error> {
        if stream_names.is_empty() {
            return Ok(());
        }

        // Create a request to commit the streams
        let commit_request = proto::BatchCommitWriteStreamsRequest {
            parent: self.parent_path.clone(),
            write_streams: stream_names.clone(),
        };

        // The uncompressed size is approximate here
        let uncompressed_size = commit_request.encoded_len();
        
        // Create a request with empty metadata/finalizers
        let request = super::service::BigqueryRequest {
            request_type: BigqueryRequestType::BatchCommitWriteStreams(commit_request),
            metadata: Default::default(),
            finalizers: Default::default(),
            uncompressed_size,
        };

        // Send the request to commit the streams
        let mut service = self.service.clone();
        match service.call(request).await {
            Ok(_) => {
                debug!(message = "Committed write streams", stream_count = %stream_names.len());
                Ok(())
            },
            Err(err) => {
                error!(message = "Failed to commit write streams", error = ?err);
                Err(format!("Failed to commit write streams: {:?}", err).into())
            }
        }
    }

    async fn run_default_stream(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        input
            .batched(self.batcher_settings.as_byte_size_config())
            .incremental_request_builder(self.request_builder)
            .flat_map(stream::iter)
            .filter_map(|request| async move {
                match request {
                    Err(error) => {
                        emit!(SinkRequestBuildError { error });
                        None
                    }
                    Ok(req) => Some(req),
                }
            })
            .into_driver(self.service)
            .protocol("gRPC")
            .run()
            .await
    }

    async fn run_pending_streams(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        // Create a channel for streaming completed write streams
        let (stream_tx, mut stream_rx) = mpsc::channel::<String>(100);
        
        // Create a shared map to store the active streams
        let active_streams = Arc::new(Mutex::new(HashMap::<String, Vec<super::service::BigqueryRequest>>::new()));
        
        // Clone references for the commit task
        let service_clone = self.service.clone();
        let parent_path_clone = self.parent_path.clone();
        
        // Spawn a task to commit streams
        let commit_task = tokio::spawn(async move {
            let mut streams_to_commit = Vec::new();
            
            loop {
                // Wait for a stream to be ready or a timeout
                match timeout(Duration::from_secs(30), stream_rx.recv()).await {
                    Ok(Some(stream_name)) => {
                        // Add the stream to the list to commit
                        streams_to_commit.push(stream_name);
                        
                        // If we have enough streams, commit them
                        if streams_to_commit.len() >= 10 {
                            // Create a request to commit the streams
                            let commit_request = proto::BatchCommitWriteStreamsRequest {
                                parent: parent_path_clone.clone(),
                                write_streams: streams_to_commit.clone(),
                            };
                            
                            let uncompressed_size = commit_request.encoded_len();
                            
                            // Create a request
                            let request = super::service::BigqueryRequest {
                                request_type: BigqueryRequestType::BatchCommitWriteStreams(commit_request),
                                metadata: Default::default(),
                                finalizers: Default::default(),
                                uncompressed_size,
                            };
                            
                            // Send the request to commit the streams
                            let mut service = service_clone.clone();
                            match service.call(request).await {
                                Ok(_) => {
                                    debug!(message = "Committed write streams", stream_count = %streams_to_commit.len());
                                    streams_to_commit.clear();
                                },
                                Err(err) => {
                                    error!(message = "Failed to commit write streams", error = ?err);
                                    // Keep the streams to retry
                                }
                            }
                        }
                    },
                    Ok(None) => {
                        // Channel closed, exit the loop
                        break;
                    },
                    Err(_) => {
                        // Timeout, commit any pending streams
                        if !streams_to_commit.is_empty() {
                            // Create a request to commit the streams
                            let commit_request = proto::BatchCommitWriteStreamsRequest {
                                parent: parent_path_clone.clone(),
                                write_streams: streams_to_commit.clone(),
                            };
                            
                            let uncompressed_size = commit_request.encoded_len();
                            
                            // Create a request
                            let request = super::service::BigqueryRequest {
                                request_type: BigqueryRequestType::BatchCommitWriteStreams(commit_request),
                                metadata: Default::default(),
                                finalizers: Default::default(),
                                uncompressed_size,
                            };
                            
                            // Send the request to commit the streams
                            let mut service = service_clone.clone();
                            match service.call(request).await {
                                Ok(_) => {
                                    debug!(message = "Committed write streams after timeout", stream_count = %streams_to_commit.len());
                                    streams_to_commit.clear();
                                },
                                Err(err) => {
                                    error!(message = "Failed to commit write streams after timeout", error = ?err);
                                    // Keep the streams to retry
                                }
                            }
                        }
                    }
                }
            }
            
            // Commit any remaining streams before exiting
            if !streams_to_commit.is_empty() {
                // Create a request to commit the streams
                let commit_request = proto::BatchCommitWriteStreamsRequest {
                    parent: parent_path_clone.clone(),
                    write_streams: streams_to_commit.clone(),
                };
                
                let uncompressed_size = commit_request.encoded_len();
                
                // Create a request
                let request = super::service::BigqueryRequest {
                    request_type: BigqueryRequestType::BatchCommitWriteStreams(commit_request),
                    metadata: Default::default(),
                    finalizers: Default::default(),
                    uncompressed_size,
                };
                
                // Send the request to commit the streams
                let mut service = service_clone.clone();
                match service.call(request).await {
                    Ok(_) => {
                        debug!(message = "Committed final write streams", stream_count = %streams_to_commit.len());
                    },
                    Err(err) => {
                        error!(message = "Failed to commit final write streams", error = ?err);
                    }
                }
            }
        });
        
        // Process the input events
        let result = input
            .batched(self.batcher_settings.as_byte_size_config())
            .then(|events| {
                let stream_tx = stream_tx.clone();
                let sink_ref = &self;
                let active_streams = Arc::clone(&active_streams);
                
                async move {
                    // Create a new pending stream
                    let stream_name = match sink_ref.create_pending_stream().await {
                        Ok(name) => name,
                        Err(err) => {
                            error!(message = "Failed to create pending stream", error = ?err);
                            return (events, Vec::new() as Vec<super::service::BigqueryRequest>);
                        }
                    };
                    
                    // Update the request builder to use this stream
                    let mut request_builder = sink_ref.request_builder.clone();
                    request_builder.write_stream = stream_name.clone();
                    
                    // Build requests for the batch of events
                    let requests_result = request_builder.encode_events_incremental(events)
                        .into_iter()
                        .filter_map(|result| {
                            match result {
                                Ok((metadata, payload)) => {
                                    Some(request_builder.build_request(metadata, payload))
                                },
                                Err(error) => {
                                    emit!(SinkRequestBuildError { error });
                                    None
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                    
                    // Store the requests in the active streams map
                    {
                        let mut streams = active_streams.lock().unwrap();
                        streams.insert(stream_name.clone(), requests_result.clone());
                    }
                    
                    // Process each request
                    let mut service = sink_ref.service.clone();
                    for request in &requests_result {
                        if let Err(err) = service.call(request.clone()).await {
                            error!(message = "Failed to append rows to stream", stream = %stream_name, error = ?err);
                        }
                    }
                    
                    // Finalize the stream
                    if let Err(err) = sink_ref.finalize_stream(stream_name.clone()).await {
                        error!(message = "Failed to finalize stream", stream = %stream_name, error = ?err);
                    }
                    
                    // Send the stream name to the commit task
                    if let Err(err) = stream_tx.send(stream_name.clone()).await {
                        error!(message = "Failed to send stream for committing", stream = %stream_name, error = ?err);
                    }
                    
                    // Remove the stream from active streams
                    {
                        let mut streams = active_streams.lock().unwrap();
                        streams.remove(&stream_name);
                    }
                    
                    (Vec::new() as Vec<Event>, requests_result)
                }
            })
            .flat_map(|(_, requests)| stream::iter(requests))
            .into_driver(self.service.clone())
            .protocol("gRPC")
            .run()
            .await;
        
        // Wait for the commit task to complete
        drop(stream_tx);
        if let Err(err) = commit_task.await {
            error!(message = "Commit task failed", error = ?err);
        }
        
        result
    }

    async fn run_inner(self: Box<BigquerySink>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        if self.use_pending_streams {
            self.run_pending_streams(input).await
        } else {
            self.run_default_stream(input).await
        }
    }
}

#[async_trait::async_trait]
impl StreamSink<Event> for BigquerySink {
    async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.run_inner(input).await
    }
}
