// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*! Functionality for signing data. */

use crate::asn1::rfc5652::DigestAlgorithmIdentifiers;
use {
    crate::{
        algorithm::{DigestAlgorithm, SignatureAlgorithm, SigningKey},
        asn1::{
            common::UtcTime,
            rfc5652::{
                Attribute, AttributeValue, CMSVersion, CertificateChoices, CertificateSet,
                DigestAlgorithmIdentifier, EncapsulatedContentInfo, IssuerAndSerialNumber,
                SignatureAlgorithmIdentifier, SignatureValue, SignedAttributes, SignedData,
                SignerIdentifier, SignerInfo, SignerInfos, OID_CONTENT_TYPE, OID_ID_DATA,
                OID_ID_SIGNED_DATA, OID_MESSAGE_DIGEST, OID_SIGNING_TIME,
            },
        },
        certificate::Certificate,
        CmsError,
    },
    bcder::{
        encode::{PrimitiveContent, Values},
        Captured, Mode, OctetString, Oid,
    },
    bytes::Bytes,
    std::collections::HashSet,
};

/// Builder type to construct an entity that will sign some data.
///
/// Instances will be attached to `SignedDataBuilder` instances where they
/// will sign data using configured settings.
pub struct SignerBuilder<'a> {
    /// The cryptographic key pair used for signing content.
    signing_key: &'a SigningKey,

    /// X.509 certificate used for signing.
    signing_certificate: Certificate,

    /// Content digest algorithm to use.
    digest_algorithm: DigestAlgorithm,

    /// Explicit content to use for calculating the `message-id`
    /// attribute.
    message_id_content: Option<Vec<u8>>,

    /// The content type of the value being signed.
    ///
    /// This is a mandatory field for signed attributes. The default value
    /// is `id-data`.
    content_type: Oid,

    /// Extra attributes to include in the SignedAttributes set.
    extra_signed_attributes: Vec<Attribute>,
}

impl<'a> SignerBuilder<'a> {
    /// Construct a new entity that will sign content.
    ///
    /// An entity is constructed from a signing key, which is mandatory.
    pub fn new(signing_key: &'a SigningKey, signing_certificate: Certificate) -> Self {
        Self {
            signing_key,
            signing_certificate,
            digest_algorithm: DigestAlgorithm::Sha256,
            message_id_content: None,
            content_type: Oid(Bytes::copy_from_slice(OID_ID_DATA.as_ref())),
            extra_signed_attributes: Vec::new(),
        }
    }

    /// Obtain the signature algorithm used by the signing key.
    pub fn signature_algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::from(self.signing_key)
    }

    /// Define the content to use to calculate the `message-id` attribute.
    ///
    /// In most cases, this is never called and the encapsulated content
    /// embedded within the generated message is used. However, some users
    /// omit storing the data inline and instead use a `message-id` digest
    /// calculated from a different source. This defines that different source.
    pub fn message_id_content(mut self, data: Vec<u8>) -> Self {
        self.message_id_content = Some(data);
        self
    }

    /// Define the content type of the signed content.
    pub fn content_type(mut self, oid: Oid) -> Self {
        self.content_type = oid;
        self
    }

    /// Add an additional attribute to sign.
    pub fn signed_attribute(mut self, typ: Oid, values: Vec<AttributeValue>) -> Self {
        self.extra_signed_attributes.push(Attribute { typ, values });
        self
    }

    /// Add an additional OctetString signed attribute.
    ///
    /// This is a helper for converting a byte slice to an OctetString and AttributeValue
    /// without having to go through low-level ASN.1 code.
    pub fn signed_attribute_octet_string(self, typ: Oid, data: &[u8]) -> Self {
        self.signed_attribute(
            typ,
            vec![AttributeValue::new(Captured::from_values(
                Mode::Der,
                data.encode_ref(),
            ))],
        )
    }
}

/// Entity for incrementally deriving a SignedData primitive.
///
/// Use this type for generating an RFC 5652 payload for signed data.
#[derive(Default)]
pub struct SignedDataBuilder<'a> {
    /// Encapsulated content to sign.
    signed_content: Option<Vec<u8>>,

    /// Entities who will generated signatures.
    signers: Vec<SignerBuilder<'a>>,

    /// X.509 certificates to add to the payload.
    certificates: Vec<crate::asn1::rfc5280::Certificate>,
}

impl<'a> SignedDataBuilder<'a> {
    /// Define the content to sign.
    ///
    /// This content will be embedded in the generated payload.
    pub fn signed_content(mut self, data: Vec<u8>) -> Self {
        self.signed_content = Some(data);
        self
    }

    /// Add a signer.
    ///
    /// The signer is the thing generating the cryptographic signature over
    /// data to be signed.
    pub fn signer(mut self, signer: SignerBuilder<'a>) -> Self {
        self.signers.push(signer);
        self
    }

    /// Add a certificate as defined by parsed ASN.1.
    pub fn certificate_asn1(mut self, cert: crate::asn1::rfc5280::Certificate) -> Self {
        if !self.certificates.iter().any(|x| x == &cert) {
            self.certificates.push(cert);
        }

        self
    }

    /// Add a certificate defined by our crate's Certificate type.
    pub fn certificate(self, cert: Certificate) -> Result<Self, CmsError> {
        if let Some(cert) = cert.raw_certificate() {
            Ok(self.certificate_asn1(cert.clone()))
        } else {
            Err(CmsError::CertificateMissingData)
        }
    }

    /// Add multiple certificates to the certificates chain.
    pub fn certificates(
        mut self,
        certs: impl Iterator<Item = Certificate>,
    ) -> Result<Self, CmsError> {
        for cert in certs {
            if let Some(cert) = cert.raw_certificate() {
                if !self.certificates.iter().any(|x| x == cert) {
                    self.certificates.push(cert.clone());
                }
            } else {
                return Err(CmsError::CertificateMissingData);
            }
        }

        Ok(self)
    }

    /// Construct a BER-encoded ASN.1 document containing a `SignedData` object.
    pub fn build_ber(&self) -> Result<Vec<u8>, CmsError> {
        let mut signer_infos = SignerInfos::default();
        let mut seen_digest_algorithms = HashSet::new();
        let mut seen_certificates = self.certificates.clone();

        for signer in &self.signers {
            seen_digest_algorithms.insert(signer.digest_algorithm);

            if let Some(cert) = signer.signing_certificate.raw_certificate() {
                if !seen_certificates.iter().any(|x| x == cert) {
                    seen_certificates.push(cert.clone());
                }
            }

            let version = CMSVersion::V1;
            let digest_algorithm = DigestAlgorithmIdentifier {
                algorithm: signer.digest_algorithm.into(),
                parameters: None,
            };

            let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
                issuer: signer.signing_certificate.issuer().clone(),
                serial_number: signer.signing_certificate.serial_number().clone(),
            });

            let mut signed_attributes = SignedAttributes::default();

            // The content-type field is mandatory.
            signed_attributes.push(Attribute {
                typ: Oid(Bytes::copy_from_slice(OID_CONTENT_TYPE.as_ref())),
                values: vec![AttributeValue::new(Captured::from_values(
                    Mode::Der,
                    signer.content_type.encode_ref(),
                ))],
            });

            // The message digest attribute is mandatory.
            //
            // Message digest is computed from override content on the signer
            // or the encapsulated content if present. The "empty" hash is a
            // valid value if no content (only signed attributes) are being signed.
            let mut hasher = signer.digest_algorithm.as_hasher();
            if let Some(content) = &signer.message_id_content {
                hasher.update(content);
            } else if let Some(content) = &self.signed_content {
                hasher.update(content);
            }

            signed_attributes.push(Attribute {
                typ: Oid(Bytes::copy_from_slice(OID_MESSAGE_DIGEST.as_ref())),
                values: vec![AttributeValue::new(Captured::from_values(
                    Mode::Der,
                    hasher.finish().as_ref().encode(),
                ))],
            });

            // Add signing time because it is common to include.
            signed_attributes.push(Attribute {
                typ: Oid(Bytes::copy_from_slice(OID_SIGNING_TIME.as_ref())),
                values: vec![AttributeValue::new(Captured::from_values(
                    Mode::Der,
                    UtcTime::now().encode(),
                ))],
            });

            signed_attributes.extend(signer.extra_signed_attributes.iter().cloned());

            let signed_attributes = Some(signed_attributes);

            let signature_algorithm = SignatureAlgorithmIdentifier {
                algorithm: signer.signature_algorithm().into(),
                parameters: None,
            };

            // The function for computing the signed attributes digested content
            // is on SignerInfo. So construct an instance so we can compute the
            // signature.
            let mut signer_info = SignerInfo {
                version,
                sid,
                digest_algorithm,
                signed_attributes,
                signature_algorithm,
                signature: SignatureValue::new(Bytes::copy_from_slice(&[])),
                unsigned_attributes: None,
                signed_attributes_data: None,
            };

            // The signature is computed over content embedded in the final message.
            // This content is the optional encapsulated content plus the DER
            // serialized signed attributes.
            let mut signed_content = Vec::new();
            if let Some(encapsulated) = &self.signed_content {
                signed_content.extend(encapsulated);
            }
            if let Some(attributes_data) = signer_info.signed_attributes_digested_content()? {
                signed_content.extend(attributes_data);
            }

            signer_info.signature =
                SignatureValue::new(Bytes::from(signer.signing_key.sign(&signed_content)?));

            signer_infos.push(signer_info);
        }

        let mut digest_algorithms = DigestAlgorithmIdentifiers::default();
        digest_algorithms.extend(seen_digest_algorithms.into_iter().map(|alg| {
            DigestAlgorithmIdentifier {
                algorithm: alg.into(),
                parameters: None,
            }
        }));

        let mut certificates = CertificateSet::default();
        certificates.extend(
            seen_certificates
                .into_iter()
                .map(|cert| CertificateChoices::Certificate(Box::new(cert))),
        );

        let signed_data = SignedData {
            version: CMSVersion::V1,
            digest_algorithms,
            content_info: EncapsulatedContentInfo {
                content_type: Oid(Bytes::copy_from_slice(OID_ID_SIGNED_DATA.as_ref())),
                content: if let Some(content) = &self.signed_content {
                    Some(OctetString::new(Bytes::copy_from_slice(content)))
                } else {
                    None
                },
            },
            certificates: if certificates.is_empty() {
                None
            } else {
                Some(certificates)
            },
            crls: None,
            signer_infos,
        };

        let mut ber = Vec::new();
        signed_data
            .encode_ref()
            .write_encoded(Mode::Ber, &mut ber)?;

        Ok(ber)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, ring::signature::UnparsedPublicKey};

    const RSA_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
        MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQC2rF88ecfP3lsn\n\
        i21jnGm7IqMG4RyG5nuXlyqmjZdvOW5tjonRyjxFJucp8GyppKwssEVuG4ohmDYi\n\
        pNdHcMjVx1rMplE6FZTvRC7RuFgmFY0PLddDFtFqUi2Z1RCkW/+Q8ebRRlhr4Pj/\n\
        qGsKDzHIgcmADOXzIqzlO+lA9xodxCfT6ay0cjG1WL1+Agf7ngy7OvVr/CDf4pbv\n\
        ooHZ9e+SZmTs1/gXVQDvEZcCk7hH12HBb7I/NHDucOEE7kJklXVGuwb5+Mhw/gKo\n\
        LEcZ644K6Jac8AH9NVM6MdNMxyZt6pR0q08oqeozP+YoIhDrtlRLkRMzw3VS2/v1\n\
        0xh+7SDzAgMBAAECggEBAI8IKs3cgPKnJXKyPmW3jCYl+caiLscF4xIQIConRcKm\n\
        EmwgJpOoqUZwLqJtCXhPYyzenI6Za6/gUcsQjSv4CJkzLkp9k65KRcKO/aXilMrF\n\
        Jx0ShLGYRULds6z24r/+9P4WGugUD5nwnqb3xVAsE4vu68qizs5wgTZAkeP3V3Cj\n\
        2usyWKuLjbXoeR/wuRluq2Q07QXHTjrVziw2JwISn5w6ynHw4ogGDxmIMoAcThiq\n\
        rTNufGA3pmBxq0Sk8umXVRjUBeoKKo/qGpfoxSDzrTxn3wt5gVRpit+oKnxTy2B7\n\
        vwC4+ASo9HEeQX0L6HJBTIxUSsgzeWnf25T+fquhyAkCgYEA2sWEsktyRQMHygjZ\n\
        S6Lb/V4ZsbJwfix6hm7//wbMFDzgtDKSRMp+C265kRf/hdYnyGQDTtan6w9GFsvO\n\
        V12CugxdC07gt2mmikWf9um716X9u5nrEgJvNotwmW1mk28rP55nr/SsKniNkx6y\n\
        JgLjGzVa2Yf9jP0A3+ASYKqFisUCgYEA1cJIuOhnBZGBBdqxG/YPljYmoaAXSrUu\n\
        raZA8a9KeZ/QODWsZwCCGA+OQZIfoLn9WueZf3oRxpIqNSqXW2XE7Xv78Ih01xLN\n\
        d7nzMSTz3GiNv1UNYmm4ZsKf/XDapYCM23oqiNcVw7XBEr1hit1IRB5slm4gESWf\n\
        dNdjMybumFcCgYEA0SeFdfArj08WY1GSbX2GVPViG0E9y2M6wMveczNMaQzKx3yR\n\
        2rK9TrDNOKp44LudzTfQ8c7HOzOfDqxK2bvM/5JSYj1HGhMn5YorJSTRMZrAulqt\n\
        IsqxCLTHMegl6U6fSnNnLhH9h505vS3bo/uepKSd9trMzb4U1/ShnUlp4wECgYEA\n\
        lwwQo0jl85Nb3q0oVZ/MZ9Kf/bnIe6wH7gD7B01cjREW64FR7/717tafKUp+Ou7y\n\
        Tpg1aVTy1qRWWvdbuOPzAfWIk/F4zrmkoyOs6183Sto+v6L0MESQX1zL/SUP+78Y\n\
        ycZL5CJIaOE4K2vTT3MKK8hr5uiulC9HvCKvIGg0VUUCgYBNrn4+tINn6iN0c45/\n\
        0qmmNuM/lLmI5UMgGsbpR0E7zHueiNjZSkPkra8uvV7km8YWoxaCyNpQMi2r/aRp\n\
        VzRAm2HqWPLEtc+BzoVT9ySc8RuOibUH6hJ7b8/secpFQwJUBhxjnxuyKXnIdxsK\n\
        wCqqgSEHwBtdDKP/nox4H+CcMw==\n\
        -----END PRIVATE KEY-----";

    const X509_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----\n\
        MIIDkzCCAnugAwIBAgIUDNhjvv6ol8EZG5YhNniO4pAiUQEwDQYJKoZIhvcNAQEL\n\
        BQAwWTELMAkGA1UEBhMCVVMxEzARBgNVBAgMCkNhbGlmb3JuaWExEDAOBgNVBAoM\n\
        B3Rlc3RpbmcxDTALBgNVBAsMBHVuaXQxFDASBgNVBAMMC1VuaXQgVGVzdGVyMB4X\n\
        DTIxMDMxNjE2MDkyOFoXDTI2MDkwNjE2MDkyOFowWTELMAkGA1UEBhMCVVMxEzAR\n\
        BgNVBAgMCkNhbGlmb3JuaWExEDAOBgNVBAoMB3Rlc3RpbmcxDTALBgNVBAsMBHVu\n\
        aXQxFDASBgNVBAMMC1VuaXQgVGVzdGVyMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n\
        MIIBCgKCAQEAtqxfPHnHz95bJ4ttY5xpuyKjBuEchuZ7l5cqpo2XbzlubY6J0co8\n\
        RSbnKfBsqaSsLLBFbhuKIZg2IqTXR3DI1cdazKZROhWU70Qu0bhYJhWNDy3XQxbR\n\
        alItmdUQpFv/kPHm0UZYa+D4/6hrCg8xyIHJgAzl8yKs5TvpQPcaHcQn0+mstHIx\n\
        tVi9fgIH+54Muzr1a/wg3+KW76KB2fXvkmZk7Nf4F1UA7xGXApO4R9dhwW+yPzRw\n\
        7nDhBO5CZJV1RrsG+fjIcP4CqCxHGeuOCuiWnPAB/TVTOjHTTMcmbeqUdKtPKKnq\n\
        Mz/mKCIQ67ZUS5ETM8N1Utv79dMYfu0g8wIDAQABo1MwUTAdBgNVHQ4EFgQUkiWC\n\
        PwIRoykbi6mtOjWNR0X1eFEwHwYDVR0jBBgwFoAUkiWCPwIRoykbi6mtOjWNR0X1\n\
        eFEwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAAN4plkAcXZIx\n\
        4KqM5AueYqYtR1y8HAaVz+5BKAWyiQJxhktAJJr7o8Yafde7SrUMfEVGDvPa2xuG\n\
        xhx5d2L3G/FDUhHbsmM3Yp3XTGkS5VwH2nHi6x4HBEpLJZfTbbTDQgS1AdtrQg0V\n\
        VY4ph7n/F0sjJL9pmpTdRx1Z2OrwYpJfWOEIA3NDflYvby9Ubb29uVRsFWrgBijl\n\
        3NIzXHvoJ2Fd+Crkc43+wWZ55hcbwSgkC1/T1mFNzd4klwncH4Rqw2KDkEFdWKmM\n\
        CiRnpyZ52+8FW64s952/SGtMs4P3fFNnWpL3njNDnfxa+r+aWDtz12PJc5FyzlkC\n\
        P4ysBX3CuA==\n\
        -----END CERTIFICATE-----";

    fn rsa_private_key() -> SigningKey {
        let key_der = pem::parse(RSA_PRIVATE_KEY.as_bytes()).unwrap();

        SigningKey::from(ring::signature::RsaKeyPair::from_pkcs8(&key_der.contents).unwrap())
    }

    fn rsa_cert() -> Certificate {
        Certificate::from_pem(X509_CERTIFICATE.as_bytes()).unwrap()
    }

    #[test]
    fn rsa_signing_roundtrip() {
        let key = rsa_private_key();
        let cert = rsa_cert();
        let message = b"hello, world";

        let signature = key.sign(message).unwrap();

        let public_key = UnparsedPublicKey::new(
            SignatureAlgorithm::Sha256Rsa.as_verification_algorithm(),
            cert.public_key().key.clone(),
        );

        public_key.verify(message, &signature).unwrap();
    }

    #[test]
    fn simple_rsa_signature() {
        let key = rsa_private_key();
        let cert = rsa_cert();

        let signer = SignerBuilder::new(&key, cert);

        let ber = SignedDataBuilder::default()
            .signed_content(vec![42])
            .signer(signer)
            .build_ber()
            .unwrap();

        let signed_data = crate::SignedData::parse_ber(&ber).unwrap();
        assert_eq!(signed_data.signed_content(), Some(vec![42].as_ref()));

        for signer in signed_data.signers() {
            signer
                .verify_message_digest_with_signed_data(&signed_data)
                .unwrap();
            signer
                .verify_signature_with_signed_data(&signed_data)
                .unwrap();
        }
    }
}
