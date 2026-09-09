use std::fmt;
use std::io::{self, Write};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    AnthropicMessages,
    BedrockConverse,
}

impl ProviderProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::AnthropicMessages => "anthropic_messages",
            Self::BedrockConverse => "bedrock_converse",
        }
    }

    pub fn preserves_appended_message_boundaries(self) -> bool {
        matches!(self, Self::OpenAiCompatible)
    }
}

/// Identity of the exact body, without endpoint, credentials, or body content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestIdentity {
    pub protocol: ProviderProtocol,
    pub sha256: String,
    pub bytes: u64,
}

/// Serialized once, shared without copying, and never modified after admission.
#[derive(Clone)]
pub struct ExactProviderRequest {
    body: Bytes,
    identity: RequestIdentity,
}

impl fmt::Debug for ExactProviderRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExactProviderRequest")
            .field("identity", &self.identity)
            .finish()
    }
}

pub enum RequestCompileError {
    TooLarge { limit: usize },
    Serialization(serde_json::Error),
    IdentityMismatch,
}

impl RequestCompileError {
    /// Instrumentation may count serializer failures without making source
    /// diagnostics part of a public error or Debug representation.
    pub fn serialization_error(&self) -> Option<&serde_json::Error> {
        match self {
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for RequestCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { limit } => write!(f, "provider request exceeds {limit} byte limit"),
            Self::Serialization(_) => f.write_str("provider request serialization failed"),
            Self::IdentityMismatch => f.write_str("provider request identity mismatch"),
        }
    }
}

impl fmt::Debug for RequestCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for RequestCompileError {}

struct LimitedBody {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("provider request size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ExactProviderRequest {
    pub fn compile(
        body: &Value,
        protocol: ProviderProtocol,
        limit: usize,
    ) -> Result<Self, RequestCompileError> {
        let mut writer = LimitedBody {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer(&mut writer, body) {
            return Err(if writer.exceeded {
                RequestCompileError::TooLarge { limit }
            } else {
                RequestCompileError::Serialization(error)
            });
        }
        let body = Bytes::from(writer.bytes);
        let identity = RequestIdentity {
            protocol,
            sha256: format!("{:x}", Sha256::digest(&body)),
            bytes: body.len() as u64,
        };
        Ok(Self { body, identity })
    }

    /// Verify received exact bytes without parsing or reserializing them.
    /// The caller separately owns admission, protocol/profile authorization,
    /// and the execution fence; matching a hash does not authorize a send.
    pub fn verify_received(
        body: Bytes,
        expected: &RequestIdentity,
        limit: usize,
    ) -> Result<Self, RequestCompileError> {
        if body.len() > limit {
            return Err(RequestCompileError::TooLarge { limit });
        }
        if body.len() as u64 != expected.bytes
            || format!("{:x}", Sha256::digest(&body)) != expected.sha256
        {
            return Err(RequestCompileError::IdentityMismatch);
        }
        Ok(Self {
            body,
            identity: expected.clone(),
        })
    }

    pub fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn body(&self) -> Bytes {
        self.body.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialized_protocol_uses_the_canonical_wire_label() {
        for protocol in [
            ProviderProtocol::OpenAiCompatible,
            ProviderProtocol::AnthropicMessages,
            ProviderProtocol::BedrockConverse,
        ] {
            let encoded = serde_json::to_value(protocol).unwrap();
            assert_eq!(encoded, protocol.as_str());
            assert_eq!(
                serde_json::from_value::<ProviderProtocol>(encoded).unwrap(),
                protocol
            );
        }
    }

    #[test]
    fn compiled_and_received_artifacts_share_exact_bytes_and_identity() {
        let value = json!({"messages": [{"content": "你好", "role": "user"}]});
        for protocol in [
            ProviderProtocol::OpenAiCompatible,
            ProviderProtocol::AnthropicMessages,
            ProviderProtocol::BedrockConverse,
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let compiled = ExactProviderRequest::compile(&value, protocol, bytes.len()).unwrap();
            assert_eq!(compiled.body(), bytes);
            assert_eq!(
                compiled.identity().sha256,
                format!("{:x}", Sha256::digest(&bytes))
            );
            let received = ExactProviderRequest::verify_received(
                compiled.body(),
                compiled.identity(),
                bytes.len(),
            )
            .unwrap();
            assert_eq!(received.identity(), compiled.identity());
            assert_eq!(received.body().as_ptr(), compiled.body().as_ptr());
        }
    }

    #[test]
    fn request_limits_and_identity_mismatch_fail_closed() {
        let value = json!({"secret": "canary-provider-secret"});
        let length = serde_json::to_vec(&value).unwrap().len();
        assert!(matches!(
            ExactProviderRequest::compile(&value, ProviderProtocol::OpenAiCompatible, length - 1),
            Err(RequestCompileError::TooLarge { .. })
        ));
        let request =
            ExactProviderRequest::compile(&value, ProviderProtocol::OpenAiCompatible, length)
                .unwrap();
        assert!(!format!("{request:?}").contains("canary-provider-secret"));
        assert!(matches!(
            ExactProviderRequest::verify_received(request.body(), request.identity(), length - 1),
            Err(RequestCompileError::TooLarge { .. })
        ));
        let mut changed = request.body().to_vec();
        changed[0] = b'[';
        assert!(matches!(
            ExactProviderRequest::verify_received(Bytes::from(changed), request.identity(), length),
            Err(RequestCompileError::IdentityMismatch)
        ));
    }
}
