use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey, certificate, rand_core::OsRng};

/// Validity window for host/client certs signed per run — comfortably beyond a typical
/// playbook run, with no mid-run renewal in v1 (see agents.md/managed-ssh design notes).
pub const CERT_VALIDITY: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(thiserror::Error, Debug)]
pub enum CaError {
    #[error(transparent)]
    SshKey(#[from] ssh_key::Error),

    #[error("system clock is set before the Unix epoch")]
    ClockError,
}

/// The operator's own SSH certificate authority. Self-managed (own Secret, own key material) —
/// generated once at operator startup if it doesn't exist yet; v1 scope has no auto-rotation.
pub struct CertificateAuthority {
    key: PrivateKey,
}

impl CertificateAuthority {
    /// Generates a brand-new CA keypair. Callers are responsible for persisting the result
    /// (`private_key_openssh`) so `from_private_key_openssh` can reload the same CA on restart.
    pub fn generate() -> Result<Self, CaError> {
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
        Ok(Self { key })
    }

    pub fn from_private_key_openssh(pem: &str) -> Result<Self, CaError> {
        Ok(Self {
            key: PrivateKey::from_openssh(pem)?,
        })
    }

    pub fn private_key_openssh(&self) -> Result<String, CaError> {
        Ok(self.key.to_openssh(LineEnding::LF)?.to_string())
    }

    /// The CA's public key in OpenSSH wire format — goes into a proxy pod's
    /// `TrustedUserCAKeys`/a client's `@cert-authority` known_hosts line. Never the private key.
    pub fn public_key_openssh(&self) -> Result<String, CaError> {
        Ok(self.key.public_key().to_openssh()?)
    }

    /// Signs a fresh host certificate for `subject_public_key`, valid as `principal` (the
    /// hostname proxied) for `CERT_VALIDITY` from now.
    pub fn sign_host_cert(
        &self,
        subject_public_key: &PublicKey,
        principal: &str,
    ) -> Result<String, CaError> {
        self.sign(subject_public_key, &[principal], certificate::CertType::Host)
    }

    /// Signs a fresh client certificate valid for every principal in `principals`, for
    /// `CERT_VALIDITY` from now. One client cert per run, trusted by every proxy pod via the
    /// shared CA — no per-pod `authorized_keys` management needed.
    ///
    /// `principals` must include the login username the proxy pods are dialed as (e.g. `"root"`,
    /// matching `PermitRootLogin yes` in `managed_ssh.rs`), since sshd's default certificate check
    /// requires that username to be in the principal list.
    pub fn sign_client_cert(
        &self,
        subject_public_key: &PublicKey,
        principals: &[&str],
    ) -> Result<String, CaError> {
        self.sign(subject_public_key, principals, certificate::CertType::User)
    }

    fn sign(
        &self,
        subject_public_key: &PublicKey,
        principals: &[&str],
        cert_type: certificate::CertType,
    ) -> Result<String, CaError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CaError::ClockError)?
            .as_secs();
        let valid_before = now + CERT_VALIDITY.as_secs();

        let mut builder =
            certificate::Builder::new_with_random_nonce(&mut OsRng, subject_public_key, now, valid_before)?;
        builder.cert_type(cert_type)?;
        for principal in principals {
            builder.valid_principal(*principal)?;
        }

        let cert = builder.sign(&self.key)?;
        Ok(cert.to_openssh()?)
    }
}

/// Generates a brand-new ephemeral ed25519 keypair (a proxy pod's host identity, or a run's
/// client identity). The operator always generates these itself — never the pod — so it never
/// has to wait for the pod to report a key back before rendering the workspace secret.
pub fn generate_ephemeral_keypair() -> Result<PrivateKey, CaError> {
    Ok(PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_host_cert() {
        let ca = CertificateAuthority::generate().unwrap();
        let host_key = generate_ephemeral_keypair().unwrap();

        let cert_openssh = ca
            .sign_host_cert(host_key.public_key(), "worker-1")
            .unwrap();

        let cert = ssh_key::Certificate::from_openssh(&cert_openssh).unwrap();
        assert_eq!(cert.cert_type(), certificate::CertType::Host);
        assert_eq!(cert.valid_principals(), &["worker-1".to_string()]);
        cert.verify_signature().unwrap();

        let ca_fingerprint = ca.key.fingerprint(ssh_key::HashAlg::Sha256);
        cert.validate([&ca_fingerprint]).unwrap();
    }

    #[test]
    fn sign_and_verify_client_cert() {
        let ca = CertificateAuthority::generate().unwrap();
        let client_key = generate_ephemeral_keypair().unwrap();

        let cert_openssh = ca
            .sign_client_cert(client_key.public_key(), &["root", "run-deadbeef"])
            .unwrap();

        let cert = ssh_key::Certificate::from_openssh(&cert_openssh).unwrap();
        assert_eq!(cert.cert_type(), certificate::CertType::User);
        assert_eq!(
            cert.valid_principals(),
            &["root".to_string(), "run-deadbeef".to_string()]
        );
        cert.verify_signature().unwrap();
    }

    #[test]
    fn cert_signed_by_a_different_ca_fails_validation() {
        let ca = CertificateAuthority::generate().unwrap();
        let other_ca = CertificateAuthority::generate().unwrap();
        let host_key = generate_ephemeral_keypair().unwrap();

        let cert_openssh = ca
            .sign_host_cert(host_key.public_key(), "worker-1")
            .unwrap();
        let cert = ssh_key::Certificate::from_openssh(&cert_openssh).unwrap();

        let other_fingerprint = other_ca.key.fingerprint(ssh_key::HashAlg::Sha256);
        assert!(cert.validate([&other_fingerprint]).is_err());
    }

    #[test]
    fn ca_key_roundtrips_through_openssh_serialization() {
        let ca = CertificateAuthority::generate().unwrap();
        let serialized = ca.private_key_openssh().unwrap();
        let reloaded = CertificateAuthority::from_private_key_openssh(&serialized).unwrap();

        assert_eq!(
            ca.public_key_openssh().unwrap(),
            reloaded.public_key_openssh().unwrap()
        );
    }
}
