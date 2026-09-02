//! The smallest DER reader that can pull a `SubjectPublicKeyInfo` and
//! the extensions out of an X.509 certificate.
//!
//! Why not `x509-parser`: it brings nine crates (`asn1-rs`, `der-parser`,
//! `nom`, `time`, …) into the *client* for two fields. The client's
//! verifier (`identity::PinnedVerifier`) needs exactly the SPKI bytes
//! and one custom extension; rustls has already checked that the
//! certificate is well-formed enough to sign the handshake with, and
//! any certificate this fails to walk is simply refused.
//!
//! Only definite-length encodings are accepted (DER forbids indefinite
//! length). Tests generate real certificates with `rcgen` and compare
//! against what it reports.

/// One TLV element.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tlv<'a> {
    pub tag: u8,
    /// The content octets.
    pub content: &'a [u8],
    /// Tag + length + content, i.e. the whole encoded element.
    pub raw: &'a [u8],
}

pub(crate) const TAG_INTEGER: u8 = 0x02;
pub(crate) const TAG_BIT_STRING: u8 = 0x03;
pub(crate) const TAG_OCTET_STRING: u8 = 0x04;
pub(crate) const TAG_OID: u8 = 0x06;
pub(crate) const TAG_SEQUENCE: u8 = 0x30;
const TAG_BOOLEAN: u8 = 0x01;
/// `[0] EXPLICIT` (the version field).
const TAG_CTX_0: u8 = 0xA0;
/// `[3] EXPLICIT` (the extensions field).
const TAG_CTX_3: u8 = 0xA3;

/// Read one element from the front of `input`; returns it and the rest.
pub(crate) fn read_tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first_len, rest) = rest.split_first()?;
    let (len, rest) = if first_len & 0x80 == 0 {
        (usize::from(first_len), rest)
    } else {
        let n = usize::from(first_len & 0x7F);
        // DER: at most four length octets are plausible for anything we
        // handle; zero means indefinite length, which DER forbids.
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | usize::from(b);
        }
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let (content, after) = rest.split_at(len);
    let raw_len = input.len() - after.len();
    Some((
        Tlv {
            tag,
            content,
            raw: &input[..raw_len],
        },
        after,
    ))
}

/// Read an element and require a specific tag.
fn expect_tlv(input: &[u8], tag: u8) -> Option<(Tlv<'_>, &[u8])> {
    let (tlv, rest) = read_tlv(input)?;
    (tlv.tag == tag).then_some((tlv, rest))
}

/// One certificate extension.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Extension<'a> {
    /// DER content octets of the OBJECT IDENTIFIER (without tag/length).
    pub oid: &'a [u8],
    /// The `extnValue` OCTET STRING contents.
    pub value: &'a [u8],
}

/// The parts of a certificate the pinning verifier looks at.
#[derive(Debug)]
pub(crate) struct Certificate<'a> {
    /// The complete `SubjectPublicKeyInfo` element, tag and length
    /// included — what the fingerprint hashes.
    pub spki: &'a [u8],
    pub extensions: Vec<Extension<'a>>,
}

/// Walk `Certificate ::= SEQUENCE { tbsCertificate, sigAlg, signature }`
/// and `TBSCertificate` far enough to find the SPKI and extensions.
pub(crate) fn parse_certificate(der: &[u8]) -> Option<Certificate<'_>> {
    let (cert, _) = expect_tlv(der, TAG_SEQUENCE)?;
    let (tbs, _) = expect_tlv(cert.content, TAG_SEQUENCE)?;
    let mut rest = tbs.content;

    // version [0] EXPLICIT — optional.
    if rest.first() == Some(&TAG_CTX_0) {
        rest = read_tlv(rest)?.1;
    }
    rest = expect_tlv(rest, TAG_INTEGER)?.1; // serialNumber
    rest = expect_tlv(rest, TAG_SEQUENCE)?.1; // signature AlgorithmIdentifier
    rest = expect_tlv(rest, TAG_SEQUENCE)?.1; // issuer
    rest = expect_tlv(rest, TAG_SEQUENCE)?.1; // validity
    rest = expect_tlv(rest, TAG_SEQUENCE)?.1; // subject
    let (spki, mut rest) = expect_tlv(rest, TAG_SEQUENCE)?;

    // Optional issuerUniqueID [1] / subjectUniqueID [2] (IMPLICIT BIT
    // STRING), then extensions [3] EXPLICIT.
    let mut extensions = Vec::new();
    while let Some((tlv, after)) = read_tlv(rest) {
        rest = after;
        if tlv.tag == TAG_CTX_3 {
            let (list, _) = expect_tlv(tlv.content, TAG_SEQUENCE)?;
            let mut items = list.content;
            while !items.is_empty() {
                let (ext, after) = expect_tlv(items, TAG_SEQUENCE)?;
                items = after;
                extensions.push(parse_extension(ext.content)?);
            }
            break;
        }
    }

    Some(Certificate {
        spki: spki.raw,
        extensions,
    })
}

/// `Extension ::= SEQUENCE { extnID OID, critical BOOLEAN DEFAULT FALSE,
/// extnValue OCTET STRING }`.
fn parse_extension(content: &[u8]) -> Option<Extension<'_>> {
    let (oid, rest) = expect_tlv(content, TAG_OID)?;
    let (next, rest) = read_tlv(rest)?;
    let value = if next.tag == TAG_BOOLEAN {
        expect_tlv(rest, TAG_OCTET_STRING)?.0
    } else if next.tag == TAG_OCTET_STRING {
        next
    } else {
        return None;
    };
    Some(Extension {
        oid: oid.content,
        value: value.content,
    })
}

/// The raw public key bytes inside a `SubjectPublicKeyInfo`: the BIT
/// STRING contents minus its leading unused-bits octet (always 0 for
/// Ed25519).
pub(crate) fn spki_public_key(spki: &[u8]) -> Option<&[u8]> {
    let (seq, _) = expect_tlv(spki, TAG_SEQUENCE)?;
    let (_alg, rest) = expect_tlv(seq.content, TAG_SEQUENCE)?;
    let (bits, _) = expect_tlv(rest, TAG_BIT_STRING)?;
    let (&unused, key) = bits.content.split_first()?;
    (unused == 0).then_some(key)
}

/// Encode an OID's arcs as DER content octets, for comparison against
/// [`Extension::oid`].
pub(crate) fn encode_oid(arcs: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some((&first, rest)) = arcs.split_first() else {
        return out;
    };
    let Some((&second, rest)) = rest.split_first() else {
        return out;
    };
    push_base128(&mut out, first * 40 + second);
    for &arc in rest {
        push_base128(&mut out, arc);
    }
    out
}

fn push_base128(out: &mut Vec<u8>, mut value: u64) {
    let mut bytes = [0u8; 10];
    let mut i = bytes.len();
    loop {
        i -= 1;
        bytes[i] = u8::try_from(value & 0x7F).unwrap_or(0);
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    let last = bytes.len() - 1;
    for (j, b) in bytes[i..].iter().enumerate() {
        let pos = i + j;
        out.push(if pos == last { *b } else { *b | 0x80 });
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, CustomExtension, KeyPair, PKCS_ED25519, PublicKeyData};

    use super::*;

    const OID: &[u64] = &[2, 25, 3_276_867_917_789_066_850];

    fn sample_cert() -> (Vec<u8>, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ED25519).expect("keygen");
        let mut params = CertificateParams::new(vec!["baucord".to_string()]).expect("params");
        params
            .custom_extensions
            .push(CustomExtension::from_oid_content(OID, vec![1, 2, 3, 4]));
        let cert = params.self_signed(&key).expect("sign");
        (cert.der().to_vec(), key)
    }

    #[test]
    fn parse_certificate_should_find_the_same_spki_rcgen_reports() {
        let (der, key) = sample_cert();
        let cert = parse_certificate(&der).expect("walkable");
        assert_eq!(cert.spki, key.subject_public_key_info().as_slice());
    }

    #[test]
    fn parse_certificate_should_expose_the_custom_extension() {
        let (der, _) = sample_cert();
        let cert = parse_certificate(&der).expect("walkable");
        let want = encode_oid(OID);
        let ext = cert
            .extensions
            .iter()
            .find(|e| e.oid == want.as_slice())
            .expect("custom extension present");
        assert_eq!(ext.value, &[1, 2, 3, 4]);
    }

    #[test]
    fn spki_public_key_should_be_the_raw_32_bytes() {
        let (_, key) = sample_cert();
        let spki = key.subject_public_key_info();
        assert_eq!(spki_public_key(&spki), Some(key.public_key_raw()));
    }

    #[test]
    fn encode_oid_matches_known_encodings() {
        // 1.2.840.113549 (RSA) = 2A 86 48 86 F7 0D
        assert_eq!(
            encode_oid(&[1, 2, 840, 113_549]),
            vec![0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D]
        );
        // 1.3.101.112 (Ed25519) = 2B 65 70
        assert_eq!(encode_oid(&[1, 3, 101, 112]), vec![0x2B, 0x65, 0x70]);
    }

    #[test]
    fn truncated_input_is_rejected_not_panicked() {
        let (der, _) = sample_cert();
        for cut in [0, 1, 5, der.len() / 2, der.len() - 1] {
            assert!(parse_certificate(&der[..cut]).is_none(), "cut at {cut}");
        }
        assert!(read_tlv(&[0x30, 0x84]).is_none());
        assert!(read_tlv(&[0x30, 0x80]).is_none());
    }
}
