//! Work with HTTP bodies as streams of Kubernetes resources.

use super::multi_response_decoder::MultiResponseDecoder;
use crate::internal_events::kubernetes::stream as internal_events;
use async_stream::try_stream;
use bytes05::Buf;
use futures::pin_mut;
use futures::stream::Stream;
use hyper::body::HttpBody as Body;
use k8s_openapi::{Response, ResponseError};
use snafu::{ResultExt, Snafu};

/// Converts the HTTP response [`Body`] to a stream of parsed Kubernetes
/// [`Response`]s.
pub fn body<B, T>(body: B) -> impl Stream<Item = Result<T, Error<<B as Body>::Error>>>
where
    T: Response + Unpin + 'static,
    B: Body,
    <B as Body>::Error: std::error::Error + 'static + Unpin,
{
    try_stream! {
        let mut decoder: MultiResponseDecoder<T> = MultiResponseDecoder::new();

        debug!(message = "streaming the HTTP body");

        pin_mut!(body);
        while let Some(buf) = body.data().await {
            let mut buf = buf.context(Reading)?;
            let chunk = buf.to_bytes();
            let responses = decoder.process_next_chunk(chunk.as_ref());
            emit!(internal_events::ChunkProcessed{ byte_size: chunk.len() });
            for response in responses {
                // Sometimes Kubernetes API starts returning `null`s in
                // the object field while streaming the response.
                // Handle it as if the stream has ended.
                // See https://github.com/kubernetes/client-go/issues/334
                if let Err(ResponseError::Json(error)) = &response {
                    if error.is_data() {
                        warn!(message = "handling response json parsing data error as steram end", ?error);
                        return;
                    }
                }
                let response = response.context(Parsing)?;
                yield response;
            }
        }
        decoder.finish().map_err(|data| Error::UnparsedDataUponCompletion { data })?;
    }
}

/// Errors that can occur in the stream.
#[derive(Debug, Snafu)]
pub enum Error<ReadError>
where
    ReadError: std::error::Error + 'static,
{
    /// An error occured while reading the response body.
    #[snafu(display("reading the data chunk failed"))]
    Reading {
        /// The error we got while reading.
        source: ReadError,
    },

    /// An error occured while parsing the response body.
    #[snafu(display("data parsing failed"))]
    Parsing {
        /// Response parsing error.
        source: ResponseError,
    },

    /// An incomplete response remains in the buffer, but we don't expect
    /// any more data.
    #[snafu(display("unparsed data remaining upon completion"))]
    UnparsedDataUponCompletion {
        /// The unparsed data.
        data: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;
    use futures::StreamExt;
    use k8s_openapi::{api::core::v1::Pod, WatchResponse};

    fn hyper_body_from_chunks(
        chunks: Vec<Result<&'static str, std::io::Error>>,
    ) -> hyper::body::Body {
        let in_stream = futures::stream::iter(chunks);
        hyper::body::Body::wrap_stream(in_stream)
    }

    #[test]
    fn test_body() {
        test_util::trace_init();
        test_util::block_on_std(async move {
            let data = r#"{
                "type": "ADDED",
                "object": {
                    "kind": "Pod",
                    "apiVersion": "v1",
                    "metadata": {
                        "uid": "uid0"
                    }
                }
            }"#;
            let chunks: Vec<Result<_, std::io::Error>> = vec![Ok(data)];
            let sample_body = hyper_body_from_chunks(chunks);

            let out_stream = body::<_, WatchResponse<Pod>>(sample_body);
            pin_mut!(out_stream);

            assert!(out_stream.next().await.unwrap().is_ok());
            assert!(out_stream.next().await.is_none());
        })
    }

    #[test]
    fn test_body_passes_reading_error() {
        test_util::trace_init();
        test_util::block_on_std(async move {
            let err = std::io::Error::new(std::io::ErrorKind::Other, "test error");
            let chunks: Vec<Result<_, std::io::Error>> = vec![Err(err)];
            let sample_body = hyper_body_from_chunks(chunks);

            let out_stream = body::<_, WatchResponse<Pod>>(sample_body);
            pin_mut!(out_stream);

            {
                let err = out_stream.next().await.unwrap().unwrap_err();
                assert!(matches!(err, Error::Reading { source: hyper::Error { .. } }));
            }

            assert!(out_stream.next().await.is_none());
        })
    }

    #[test]
    fn test_body_passes_parsing_error() {
        test_util::trace_init();
        test_util::block_on_std(async move {
            let chunks: Vec<Result<_, std::io::Error>> = vec![Ok("qwerty")];
            let sample_body = hyper_body_from_chunks(chunks);

            let out_stream = body::<_, WatchResponse<Pod>>(sample_body);
            pin_mut!(out_stream);

            {
                let err = out_stream.next().await.unwrap().unwrap_err();
                assert!(matches!(err, Error::Parsing { source: ResponseError::Json(_) }));
            }

            assert!(out_stream.next().await.is_none());
        })
    }

    #[test]
    fn test_body_uses_finish() {
        test_util::trace_init();
        test_util::block_on_std(async move {
            let chunks: Vec<Result<_, std::io::Error>> = vec![Ok("{")];
            let sample_body = hyper_body_from_chunks(chunks);

            let out_stream = body::<_, WatchResponse<Pod>>(sample_body);
            pin_mut!(out_stream);

            {
                let err = out_stream.next().await.unwrap().unwrap_err();
                assert!(matches!(
                    err,
                    Error::UnparsedDataUponCompletion { data } if data == vec![b'{']
                ));
            }

            assert!(out_stream.next().await.is_none());
        })
    }

    #[test]
    fn test_sudden_null() {
        test_util::trace_init();
        test_util::block_on_std(async move {
            let chunks: Vec<Result<_, std::io::Error>> = vec![Ok("null")];
            let sample_body = hyper_body_from_chunks(chunks);

            let out_stream = body::<_, WatchResponse<Pod>>(sample_body);
            pin_mut!(out_stream);

            assert!(out_stream.next().await.is_none());
        })
    }
}
