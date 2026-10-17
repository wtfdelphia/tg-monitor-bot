use aws_sdk_kms::config::endpoint::{DefaultResolver, EndpointFuture, Params, ResolveEndpoint};
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::{Client, config::Region};
use tgm_core::kek::{DEK_LEN, Kek, KekError};
use tgm_core::secret::Secret;
use zeroize::Zeroize;

pub struct KmsKek {
    client: Client,
    key_arn: String,
}

impl KmsKek {
    pub async fn from_arn(key_arn: &str) -> Result<Self, KekError> {
        let shared_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = client_from_shared_config(&shared_config, key_arn)?;
        Ok(Self {
            client,
            key_arn: key_arn.to_owned(),
        })
    }

    #[cfg(test)]
    fn with_client(client: Client, key_arn: &str) -> Result<Self, KekError> {
        key_arn_parts(key_arn).ok_or(KekError::Crypto)?;
        Ok(Self {
            client,
            key_arn: key_arn.to_owned(),
        })
    }
}

fn client_from_shared_config(
    shared_config: &aws_config::SdkConfig,
    key_arn: &str,
) -> Result<Client, KekError> {
    let (_, region) = key_arn_parts(key_arn).ok_or(KekError::Crypto)?;
    let config = aws_sdk_kms::config::Builder::from(shared_config)
        .region(Region::new(region.to_owned()))
        .endpoint_resolver(RegionalKmsEndpointResolver)
        .build();
    Ok(Client::from_conf(config))
}

#[derive(Debug)]
struct RegionalKmsEndpointResolver;

impl ResolveEndpoint for RegionalKmsEndpointResolver {
    fn resolve_endpoint<'a>(&'a self, params: &'a Params) -> EndpointFuture<'a> {
        let Some(region) = params.region() else {
            return EndpointFuture::ready(Err("KMS 区域缺失".into()));
        };
        let resolved_params = Params::builder()
            .region(region.to_owned())
            .use_dual_stack(params.use_dual_stack().unwrap_or(false))
            .use_fips(params.use_fips().unwrap_or(false))
            .build();
        match resolved_params {
            Ok(resolved_params) => EndpointFuture::new(async move {
                DefaultResolver::new()
                    .resolve_endpoint(&resolved_params)
                    .await
            }),
            Err(_) => EndpointFuture::ready(Err("KMS 区域配置无效".into())),
        }
    }
}

#[async_trait::async_trait]
impl Kek for KmsKek {
    async fn wrap(&self, dek: &Secret<[u8; DEK_LEN]>) -> Result<Vec<u8>, KekError> {
        let output = self
            .client
            .encrypt()
            .key_id(&self.key_arn)
            .plaintext(Blob::new(dek.expose().to_vec()))
            .send()
            .await
            .map_err(|_| KekError::Crypto)?;
        output
            .ciphertext_blob()
            .map(|blob| blob.as_ref().to_vec())
            .ok_or(KekError::Crypto)
    }

    async fn unwrap(&self, ciphertext: &[u8]) -> Result<Secret<[u8; DEK_LEN]>, KekError> {
        let output = self
            .client
            .decrypt()
            .key_id(&self.key_arn)
            .ciphertext_blob(Blob::new(ciphertext.to_vec()))
            .send()
            .await
            .map_err(|_| KekError::Crypto)?;
        let mut plaintext = output.plaintext.ok_or(KekError::Crypto)?.into_inner();
        if plaintext.len() != DEK_LEN {
            plaintext.zeroize();
            return Err(KekError::Crypto);
        }
        let mut dek = [0; DEK_LEN];
        dek.copy_from_slice(&plaintext);
        plaintext.zeroize();
        Ok(Secret::new(dek))
    }

    fn kek_id(&self) -> &str {
        &self.key_arn
    }
}

fn key_arn_parts(key_arn: &str) -> Option<(&str, &str)> {
    let mut parts = key_arn.splitn(6, ':');
    let arn = parts.next()?;
    let partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    let account = parts.next()?;
    let resource = parts.next()?;
    (arn == "arn"
        && matches!(
            partition,
            "aws" | "aws-cn" | "aws-us-gov" | "aws-iso" | "aws-iso-b"
        )
        && service == "kms"
        && !region.is_empty()
        && account.len() == 12
        && account.bytes().all(|byte| byte.is_ascii_digit())
        && resource.starts_with("key/")
        && resource.len() > "key/".len()
        && partition_for_region(region) == partition)
        .then_some((partition, region))
}

fn partition_for_region(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "aws-cn"
    } else if region.starts_with("us-gov-") {
        "aws-us-gov"
    } else if region.starts_with("us-iso-") {
        "aws-iso"
    } else if region.starts_with("us-isob-") {
        "aws-iso-b"
    } else {
        "aws"
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use aws_sdk_kms::config::{Credentials, Region};
    use aws_smithy_runtime_api::client::http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    };
    use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
    use aws_smithy_runtime_api::http::StatusCode;
    use aws_smithy_types::body::SdkBody;
    use aws_types::origin::Origin;
    use aws_types::service_config::{LoadServiceConfig, ServiceConfigKey};
    use tgm_core::kek::{DEK_LEN, Kek, KekError, self_test};
    use tgm_core::secret::Secret;

    use super::{
        KmsKek, RegionalKmsEndpointResolver, client_from_shared_config, key_arn_parts,
        partition_for_region,
    };

    const KEY_ARN: &str = "arn:aws:kms:eu-west-1:123456789012:key/test-key";
    const PROBE_BASE64: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

    #[test]
    fn validates_kms_key_arns_and_extracts_region() {
        assert_eq!(key_arn_parts(KEY_ARN), Some(("aws", "eu-west-1")));
        assert_eq!(
            key_arn_parts("arn:aws-cn:kms:cn-north-1:123456789012:key/key-id"),
            Some(("aws-cn", "cn-north-1"))
        );
        for invalid in [
            "",
            "arn:aws:kms::123456789012:key/test-key",
            "arn:aws:kms:eu-west-1:123456789012:alias/test-key",
            "arn:aws:kms:eu-west-1:123:key/test-key",
            "arn:aws-cn:kms:eu-west-1:123456789012:key/test-key",
            "arn:aws:kms:cn-north-1:123456789012:key/test-key",
        ] {
            assert_eq!(key_arn_parts(invalid), None, "{invalid:?}");
        }
        assert_eq!(partition_for_region("ap-southeast-2"), "aws");
        assert_eq!(partition_for_region("cn-north-1"), "aws-cn");
        assert_eq!(partition_for_region("us-gov-west-1"), "aws-us-gov");
        assert_eq!(partition_for_region("us-iso-east-1"), "aws-iso");
        assert_eq!(partition_for_region("us-isob-east-1"), "aws-iso-b");
    }

    #[tokio::test]
    async fn production_endpoint_resolver_ignores_custom_endpoints_and_uses_arn_region() {
        for (region, custom_endpoint, expected) in [
            (
                "ap-southeast-2",
                "https://attacker.invalid/kms",
                "https://kms.ap-southeast-2.amazonaws.com",
            ),
            (
                "cn-north-1",
                "http://127.0.0.1:4566",
                "https://kms.cn-north-1.amazonaws.com.cn",
            ),
            (
                "us-gov-west-1",
                "https://attacker.invalid",
                "https://kms.us-gov-west-1.amazonaws.com",
            ),
        ] {
            let params = aws_sdk_kms::config::endpoint::Params::builder()
                .region(region)
                .endpoint(custom_endpoint)
                .build()
                .expect("endpoint resolver 参数无效");
            let endpoint = aws_sdk_kms::config::endpoint::ResolveEndpoint::resolve_endpoint(
                &RegionalKmsEndpointResolver,
                &params,
            )
            .await
            .expect("KMS 区域 endpoint 解析失败");
            assert_eq!(endpoint.url(), expected, "region={region}");
        }
    }

    #[tokio::test]
    async fn production_client_ignores_global_service_and_profile_endpoint_overrides() {
        for (source, global_endpoint, global_origin, service_endpoint, profile_endpoint) in [
            (
                "global environment",
                Some("https://attacker.invalid/global"),
                Some(Origin::shared_environment_variable()),
                None,
                None,
            ),
            (
                "global profile",
                Some("https://attacker.invalid/global-profile"),
                Some(Origin::shared_profile_file()),
                None,
                None,
            ),
            (
                "KMS service environment",
                None,
                None,
                Some("https://attacker.invalid/service"),
                None,
            ),
            (
                "KMS service profile",
                None,
                None,
                None,
                Some("https://attacker.invalid/service-profile"),
            ),
        ] {
            let mut config_builder = aws_config::SdkConfig::builder()
                .behavior_version(aws_config::BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .credentials_provider(aws_sdk_kms::config::SharedCredentialsProvider::new(
                    Credentials::new(
                        "test-access-key",
                        "test-secret-key",
                        None,
                        None,
                        "kms-kek-test",
                    ),
                ));
            if let Some(endpoint) = global_endpoint {
                config_builder = config_builder.endpoint_url(endpoint);
                config_builder.insert_origin(
                    "endpoint_url",
                    global_origin.expect("全局 endpoint 必须有来源"),
                );
            }
            if service_endpoint.is_some() || profile_endpoint.is_some() {
                config_builder = config_builder.service_config(ProfileEndpoint {
                    service: service_endpoint,
                    profile: profile_endpoint,
                });
            }
            let shared_config = config_builder.build();
            let client = client_from_shared_config(&shared_config, KEY_ARN)
                .expect("创建生产配置 KMS 客户端失败");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let client = aws_sdk_kms::Client::from_conf(
                client
                    .config()
                    .to_builder()
                    .http_client(RequestCapture(Arc::clone(&requests)))
                    .retry_config(
                        aws_sdk_kms::config::retry::RetryConfig::standard().with_max_attempts(1),
                    )
                    .build(),
            );
            client
                .encrypt()
                .key_id(KEY_ARN)
                .plaintext(aws_sdk_kms::primitives::Blob::new(vec![0x5a; DEK_LEN]))
                .send()
                .await
                .unwrap_or_else(|_| panic!("mock KMS Encrypt 失败，来源：{source}"));
            let requests = requests.lock().expect("读取 mock 请求失败");
            assert_eq!(requests.len(), 1, "source={source}");
            assert_eq!(
                requests[0], "https://kms.eu-west-1.amazonaws.com/",
                "source={source}"
            );
        }
    }

    #[derive(Debug)]
    struct ProfileEndpoint {
        service: Option<&'static str>,
        profile: Option<&'static str>,
    }

    impl LoadServiceConfig for ProfileEndpoint {
        fn load_config(&self, key: ServiceConfigKey<'_>) -> Option<String> {
            if key.service_id() != "KMS" {
                return None;
            }
            if key.env() == "AWS_ENDPOINT_URL" {
                return self.service.map(str::to_owned);
            }
            if key.profile() == "endpoint_url" {
                return self.profile.map(str::to_owned);
            }
            None
        }
    }

    #[derive(Clone, Debug)]
    struct RequestCapture(Arc<Mutex<Vec<String>>>);

    impl HttpClient for RequestCapture {
        fn http_connector(
            &self,
            _: &HttpConnectorSettings,
            _: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(CaptureConnector(Arc::clone(&self.0)))
        }
    }

    #[derive(Debug)]
    struct CaptureConnector(Arc<Mutex<Vec<String>>>);

    impl HttpConnector for CaptureConnector {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            self.0
                .lock()
                .expect("记录 mock 请求失败")
                .push(request.uri().to_owned());
            HttpConnectorFuture::ready(Ok(HttpResponse::new(
                StatusCode::try_from(200).expect("HTTP 状态码无效"),
                SdkBody::from(
                    r#"{"CiphertextBlob":"AQIDBA==","KeyId":"arn:aws:kms:eu-west-1:123456789012:key/test-key"}"#,
                ),
            )))
        }
    }

    /// OCI 目标形态（Roles Anywhere 经 `credential_process`）的编译期前提：
    /// 根 `Cargo.toml` 去掉 `aws-config` 的 `credentials-process` feature 后，
    /// SDK 报「requires following cargo feature(s)」，本测试失败。
    #[tokio::test]
    async fn profile_credential_process_is_compiled_in() {
        use aws_config::profile::ProfileFileCredentialsProvider;
        use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
        use aws_sdk_kms::config::ProvideCredentials;

        let path =
            std::env::temp_dir().join(format!("tgm-credential-process-{}.ini", std::process::id()));
        std::fs::write(
            &path,
            "[default]\ncredential_process = printf '%s' '{\"Version\":1,\"AccessKeyId\":\"ASIAPROCESSTEST\",\"SecretAccessKey\":\"s\",\"SessionToken\":\"t\",\"Expiration\":\"2999-01-01T00:00:00Z\"}'\n",
        )
        .expect("写 profile fixture 失败");
        let files = EnvConfigFiles::builder()
            .with_file(EnvConfigFileKind::Config, &path)
            .build();
        let result = ProfileFileCredentialsProvider::builder()
            .profile_files(files)
            .build()
            .provide_credentials()
            .await;
        let _ = std::fs::remove_file(&path);

        let credentials = result.expect("credential_process 未生效");
        assert_eq!(credentials.access_key_id(), "ASIAPROCESSTEST");
        assert_eq!(credentials.session_token(), Some("t"));
    }

    #[tokio::test]
    async fn encrypt_decrypt_and_startup_self_test_use_kms_protocol() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock KMS 监听失败");
        let address = listener.local_addr().expect("读取 mock KMS 地址失败");
        let server = thread::spawn(move || {
            for expected_target in ["TrentService.Encrypt", "TrentService.Decrypt"] {
                let (stream, _) = listener.accept().expect("接受 mock KMS 请求失败");
                respond(stream, expected_target);
            }
        });
        let config = aws_sdk_kms::config::Builder::new()
            .behavior_version_latest()
            .region(Region::new("eu-west-1"))
            .credentials_provider(Credentials::new(
                "test-access-key",
                "test-secret-key",
                None,
                None,
                "kms-kek-test",
            ))
            .endpoint_url(format!("http://{address}"))
            .build();
        let kek = KmsKek::with_client(aws_sdk_kms::Client::from_conf(config), KEY_ARN)
            .expect("创建 mock KMS KEK 失败");
        assert_eq!(kek.kek_id(), KEY_ARN);
        self_test(&kek).await.expect("mock KMS 往返自测失败");
        server.join().expect("mock KMS 服务线程失败");
    }

    #[tokio::test]
    async fn sanitizes_kms_errors_and_rejects_wrong_plaintext_length() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock KMS 监听失败");
        let address = listener.local_addr().expect("读取 mock KMS 地址失败");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接受 mock KMS 请求失败");
            let _ = read_request(&mut stream);
            write_response(
                &mut stream,
                "400 Bad Request",
                r#"{"__type":"AccessDeniedException","message":"sensitive provider details"}"#,
            );
        });
        let kek = mock_kek(address.to_string());
        let error = kek
            .wrap(&Secret::new([0x5a; DEK_LEN]))
            .await
            .expect_err("KMS 错误应被返回");
        assert!(matches!(error, KekError::Crypto));
        assert_eq!(error.to_string(), "KEK 操作失败");
        server.join().expect("mock KMS 服务线程失败");

        let listener = TcpListener::bind("127.0.0.1:0").expect("mock KMS 监听失败");
        let address = listener.local_addr().expect("读取 mock KMS 地址失败");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接受 mock KMS 请求失败");
            let _ = read_request(&mut stream);
            write_response(
                &mut stream,
                "200 OK",
                r#"{"KeyId":"arn:aws:kms:eu-west-1:123456789012:key/test-key","Plaintext":"AQI="}"#,
            );
        });
        let error = mock_kek(address.to_string())
            .unwrap(&[1, 2, 3])
            .await
            .expect_err("非 DEK 长度的明文应拒绝");
        assert!(matches!(error, KekError::Crypto));
        server.join().expect("mock KMS 服务线程失败");
    }

    fn mock_kek(address: String) -> KmsKek {
        let config = aws_sdk_kms::config::Builder::new()
            .behavior_version_latest()
            .region(Region::new("eu-west-1"))
            .credentials_provider(Credentials::new(
                "test-access-key",
                "test-secret-key",
                None,
                None,
                "kms-kek-test",
            ))
            .endpoint_url(format!("http://{address}"))
            .build();
        KmsKek::with_client(aws_sdk_kms::Client::from_conf(config), KEY_ARN)
            .expect("创建 mock KMS KEK 失败")
    }

    fn respond(mut stream: TcpStream, expected_target: &str) {
        let (headers, body) = read_request(&mut stream);
        assert!(
            headers.contains(expected_target),
            "unexpected request: {headers}"
        );
        assert!(
            body.contains(KEY_ARN),
            "key ARN missing from request: {body}"
        );
        let response = if expected_target.ends_with("Encrypt") {
            assert!(
                body.contains(PROBE_BASE64),
                "DEK missing from request: {body}"
            );
            r#"{"CiphertextBlob":"AQIDBA==","KeyId":"arn:aws:kms:eu-west-1:123456789012:key/test-key"}"#.to_owned()
        } else {
            assert!(
                body.contains("AQIDBA=="),
                "ciphertext missing from request: {body}"
            );
            format!(r#"{{"KeyId":"{KEY_ARN}","Plaintext":"{PROBE_BASE64}"}}"#)
        };
        write_response(&mut stream, "200 OK", &response);
    }

    fn read_request(stream: &mut TcpStream) -> (String, String) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("读取 mock KMS 请求头失败");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().expect("mock KMS Content-Length 无效");
            }
            headers.push_str(&line);
        }
        let mut body = vec![0; content_length];
        reader
            .read_exact(&mut body)
            .expect("读取 mock KMS 请求体失败");
        (
            headers,
            String::from_utf8(body).expect("mock KMS 请求体不是 UTF-8"),
        )
    }

    fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: application/x-amz-json-1.1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("发送 mock KMS 响应失败");
    }
}
