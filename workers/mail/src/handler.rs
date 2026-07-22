use kastellan_protocol::{codes, server::Handler, RpcError};

pub struct MailHandler;

impl MailHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self)
    }
}

impl Handler for MailHandler {
    fn call(
        &mut self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("unknown method {method}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = MailHandler;
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }
}
