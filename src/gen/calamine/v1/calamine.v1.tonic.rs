// @generated
/// Generated client implementations.
pub mod calamine_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct CalamineServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl CalamineServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> CalamineServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> CalamineServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            CalamineServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn open_workbook(
            &mut self,
            request: impl tonic::IntoStreamingRequest<
                Message = super::OpenWorkbookRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::OpenWorkbookResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/OpenWorkbook",
            );
            let mut req = request.into_streaming_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("calamine.v1.CalamineService", "OpenWorkbook"));
            self.inner.client_streaming(req, path, codec).await
        }
        pub async fn close_workbook(
            &mut self,
            request: impl tonic::IntoRequest<super::CloseWorkbookRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseWorkbookResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/CloseWorkbook",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("calamine.v1.CalamineService", "CloseWorkbook"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_metadata(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMetadataRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMetadataResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/GetMetadata",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("calamine.v1.CalamineService", "GetMetadata"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn stream_worksheet_range(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamWorksheetRangeRequest>,
        ) -> std::result::Result<
            tonic::Response<
                tonic::codec::Streaming<super::StreamWorksheetRangeResponse>,
            >,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/StreamWorksheetRange",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "calamine.v1.CalamineService",
                        "StreamWorksheetRange",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn stream_worksheet_formula(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamWorksheetFormulaRequest>,
        ) -> std::result::Result<
            tonic::Response<
                tonic::codec::Streaming<super::StreamWorksheetFormulaResponse>,
            >,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/StreamWorksheetFormula",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "calamine.v1.CalamineService",
                        "StreamWorksheetFormula",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_defined_names(
            &mut self,
            request: impl tonic::IntoRequest<super::GetDefinedNamesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetDefinedNamesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/GetDefinedNames",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("calamine.v1.CalamineService", "GetDefinedNames"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn stream_vba_project(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamVbaProjectRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::StreamVbaProjectResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/StreamVbaProject",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("calamine.v1.CalamineService", "StreamVbaProject"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_pictures(
            &mut self,
            request: impl tonic::IntoRequest<super::GetPicturesRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::GetPicturesResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/calamine.v1.CalamineService/GetPictures",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("calamine.v1.CalamineService", "GetPictures"));
            self.inner.server_streaming(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod calamine_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with CalamineServiceServer.
    #[async_trait]
    pub trait CalamineService: std::marker::Send + std::marker::Sync + 'static {
        async fn open_workbook(
            &self,
            request: tonic::Request<tonic::Streaming<super::OpenWorkbookRequest>>,
        ) -> std::result::Result<
            tonic::Response<super::OpenWorkbookResponse>,
            tonic::Status,
        >;
        async fn close_workbook(
            &self,
            request: tonic::Request<super::CloseWorkbookRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseWorkbookResponse>,
            tonic::Status,
        >;
        async fn get_metadata(
            &self,
            request: tonic::Request<super::GetMetadataRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMetadataResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the StreamWorksheetRange method.
        type StreamWorksheetRangeStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::StreamWorksheetRangeResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn stream_worksheet_range(
            &self,
            request: tonic::Request<super::StreamWorksheetRangeRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::StreamWorksheetRangeStream>,
            tonic::Status,
        >;
        /// Server streaming response type for the StreamWorksheetFormula method.
        type StreamWorksheetFormulaStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::StreamWorksheetFormulaResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn stream_worksheet_formula(
            &self,
            request: tonic::Request<super::StreamWorksheetFormulaRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::StreamWorksheetFormulaStream>,
            tonic::Status,
        >;
        async fn get_defined_names(
            &self,
            request: tonic::Request<super::GetDefinedNamesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetDefinedNamesResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the StreamVbaProject method.
        type StreamVbaProjectStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::StreamVbaProjectResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn stream_vba_project(
            &self,
            request: tonic::Request<super::StreamVbaProjectRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::StreamVbaProjectStream>,
            tonic::Status,
        >;
        /// Server streaming response type for the GetPictures method.
        type GetPicturesStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::GetPicturesResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn get_pictures(
            &self,
            request: tonic::Request<super::GetPicturesRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::GetPicturesStream>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct CalamineServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> CalamineServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for CalamineServiceServer<T>
    where
        T: CalamineService,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/calamine.v1.CalamineService/OpenWorkbook" => {
                    #[allow(non_camel_case_types)]
                    struct OpenWorkbookSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::ClientStreamingService<super::OpenWorkbookRequest>
                    for OpenWorkbookSvc<T> {
                        type Response = super::OpenWorkbookResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                tonic::Streaming<super::OpenWorkbookRequest>,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::open_workbook(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = OpenWorkbookSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.client_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/CloseWorkbook" => {
                    #[allow(non_camel_case_types)]
                    struct CloseWorkbookSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::UnaryService<super::CloseWorkbookRequest>
                    for CloseWorkbookSvc<T> {
                        type Response = super::CloseWorkbookResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CloseWorkbookRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::close_workbook(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CloseWorkbookSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/GetMetadata" => {
                    #[allow(non_camel_case_types)]
                    struct GetMetadataSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::UnaryService<super::GetMetadataRequest>
                    for GetMetadataSvc<T> {
                        type Response = super::GetMetadataResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetMetadataRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::get_metadata(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetMetadataSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/StreamWorksheetRange" => {
                    #[allow(non_camel_case_types)]
                    struct StreamWorksheetRangeSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::ServerStreamingService<
                        super::StreamWorksheetRangeRequest,
                    > for StreamWorksheetRangeSvc<T> {
                        type Response = super::StreamWorksheetRangeResponse;
                        type ResponseStream = T::StreamWorksheetRangeStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::StreamWorksheetRangeRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::stream_worksheet_range(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = StreamWorksheetRangeSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/StreamWorksheetFormula" => {
                    #[allow(non_camel_case_types)]
                    struct StreamWorksheetFormulaSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::ServerStreamingService<
                        super::StreamWorksheetFormulaRequest,
                    > for StreamWorksheetFormulaSvc<T> {
                        type Response = super::StreamWorksheetFormulaResponse;
                        type ResponseStream = T::StreamWorksheetFormulaStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::StreamWorksheetFormulaRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::stream_worksheet_formula(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = StreamWorksheetFormulaSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/GetDefinedNames" => {
                    #[allow(non_camel_case_types)]
                    struct GetDefinedNamesSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::UnaryService<super::GetDefinedNamesRequest>
                    for GetDefinedNamesSvc<T> {
                        type Response = super::GetDefinedNamesResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetDefinedNamesRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::get_defined_names(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetDefinedNamesSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/StreamVbaProject" => {
                    #[allow(non_camel_case_types)]
                    struct StreamVbaProjectSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::ServerStreamingService<
                        super::StreamVbaProjectRequest,
                    > for StreamVbaProjectSvc<T> {
                        type Response = super::StreamVbaProjectResponse;
                        type ResponseStream = T::StreamVbaProjectStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::StreamVbaProjectRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::stream_vba_project(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = StreamVbaProjectSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/calamine.v1.CalamineService/GetPictures" => {
                    #[allow(non_camel_case_types)]
                    struct GetPicturesSvc<T: CalamineService>(pub Arc<T>);
                    impl<
                        T: CalamineService,
                    > tonic::server::ServerStreamingService<super::GetPicturesRequest>
                    for GetPicturesSvc<T> {
                        type Response = super::GetPicturesResponse;
                        type ResponseStream = T::GetPicturesStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetPicturesRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as CalamineService>::get_pictures(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetPicturesSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for CalamineServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "calamine.v1.CalamineService";
    impl<T> tonic::server::NamedService for CalamineServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
