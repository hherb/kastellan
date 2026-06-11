//! Per-instance ephemeral CA + on-demand leaf issuance for TLS interception.
//!
//! Each proxy process generates ONE CA at startup; its private key lives only
//! here (never written to disk — only the public cert PEM is exported for the
//! host to inject into the worker's trust store). Leaves are signed per-host on
//! demand and presented to the worker, which trusts only this CA. A CA
//! compromise is therefore scoped to one worker's one short-lived proxy.

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The process-lifetime CA: the public cert (PEM + DER) plus the signing
/// material (`cert` + `key_pair`) used to issue leaves. rcgen 0.13's
/// `CertificateParams::signed_by` takes the issuer's `&Certificate` and
/// `&KeyPair` directly (there is no public `Issuer` type), so we keep both
/// here. The CA `KeyPair` never leaves this process.
pub struct CaMaterial {
    cert_pem: String,
    cert: Certificate,
    key_pair: KeyPair,
}

impl CaMaterial {
    /// Public CA certificate, PEM-encoded — the only thing exported off-process.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }
}

/// A signed leaf for one host: the cert DER + its private key DER, ready to be
/// dropped into a rustls `ServerConfig::with_single_cert`.
pub struct LeafCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

impl LeafCert {
    /// The leaf certificate DER. Test-only accessor (the proxy consumes the
    /// fields via [`Self::into_rustls`]); exposed so `ca/tests.rs` can assert the
    /// SAN encoding.
    #[cfg(test)]
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }
    /// The leaf's private key DER. Test-only accessor (see [`Self::cert_der`]).
    #[cfg(test)]
    pub fn key_der(&self) -> &PrivateKeyDer<'static> {
        &self.key_der
    }
    /// Consume into the (chain, key) pair rustls' `with_single_cert` wants.
    pub fn into_rustls(self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (vec![self.cert_der], self.key_der)
    }
}

/// Generate a fresh ephemeral CA. Default rcgen validity (a wide fixed window)
/// is fine for an ephemeral per-process CA.
pub fn generate_ca() -> Result<CaMaterial, rcgen::Error> {
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "kastellan egress-proxy");
    params
        .distinguished_name
        .push(DnType::CommonName, "kastellan ephemeral egress CA");
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);

    let key_pair = KeyPair::generate()?;
    // `self_signed` consumes `params`; the resulting `Certificate` retains its
    // own copy of the params, which `signed_by` later reads to set the leaf's
    // issuer DN + authority-key-id.
    let cert = params.self_signed(&key_pair)?;
    let cert_pem = cert.pem();
    Ok(CaMaterial { cert_pem, cert, key_pair })
}

/// Issue a leaf for `host`, signed by `ca`. `host` becomes the sole SAN and the
/// CN. Server-auth EKU so rustls accepts it as a TLS server cert.
pub fn issue_leaf(ca: &CaMaterial, host: &str) -> Result<LeafCert, rcgen::Error> {
    let mut params = CertificateParams::new(vec![host.to_string()])?;
    params.distinguished_name.push(DnType::CommonName, host);
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);

    let key_pair = KeyPair::generate()?;
    // `signed_by(self, public_key, issuer_cert, issuer_key)`: the leaf's own
    // key pair is both the subject public key and (via its public half) what
    // goes in the cert; the CA cert + CA key pair sign it.
    let cert = params.signed_by(&key_pair, &ca.cert, &ca.key_pair)?;
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    Ok(LeafCert { cert_der, key_der })
}

#[cfg(test)]
mod tests;
