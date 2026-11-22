//! Konak's local certificate authority, for apps served over HTTPS (manifest `https: true`).
//! The root is generated here and handed to Caddy (`pki { ca local { root ... } } }`), which keeps
//! its own short-lived intermediate under it and issues leaves for `tls internal` sites.
//! The root carries an X.509 name constraint that permits only DNS names under `.local` and excludes
//! every IP address, so a leaked key cannot impersonate other sites on devices that trust it.

use crate::paths::Paths;
use crate::shell::{self, tail};
use rcgen::{BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose, NameConstraints, SerialNumber};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;
use time::{Duration, OffsetDateTime};

pub struct Root {
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn generate_root(common_name: &str) -> Result<Root, String> {
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut p = CertificateParams::default();
    p.distinguished_name.push(DnType::CommonName, common_name);
    p.distinguished_name.push(DnType::OrganizationName, "Konak");
    // Path length 1: Caddy's intermediate sits under the root, leaves under the intermediate, and
    // nothing below that. serverAuth only: the root cannot vouch for code signing, mail and so on.
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName("local".into())],
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([0; 4], 0)),
            GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0)),
        ],
    });
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::days(1);
    p.not_after = now + Duration::days(3650);
    // A serial derived from the new key: unique per root, and it names the cert for `certutil -delstore`.
    let mut serial = Sha256::digest(key.serialize_der())[..16].to_vec();
    serial[0] = (serial[0] & 0x7f) | 0x40; // positive, and no leading zero byte to strip in DER
    p.serial_number = Some(SerialNumber::from_slice(&serial));
    let cert = p.self_signed(&key).map_err(|e| e.to_string())?;
    Ok(Root { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

/// Current user's SID, for ACLs that do not depend on how the account name is spelled.
fn user_sid() -> Result<String, String> {
    let out = shell::run("whoami.exe", &["/user", "/fo", "csv", "/nh"]);
    out.text.trim().rsplit(',').next().map(|s| s.trim_matches('"').to_string()).filter(|s| s.starts_with("S-1-")).ok_or(format!("cannot read user SID: {}", out.text))
}

/// Removes inherited access and grants full control to the current user only.
fn restrict_to_user(path: &Path, dir: bool) -> Result<(), String> {
    let grant = format!("*{}:{}F", user_sid()?, if dir { "(OI)(CI)" } else { "" });
    let out = shell::run("icacls.exe", &[&path.to_string_lossy(), "/inheritance:r", "/grant:r", &grant]);
    if !out.ok {
        return Err(format!("icacls failed: {}", tail(&out.text, 2)));
    }
    Ok(())
}

/// Creates Caddy's storage folder, which holds the intermediate CA key Caddy derives from the root,
/// with the same current-user-only ACL as the root's folder.
pub fn prepare_caddy_storage(paths: &Paths) -> Result<(), String> {
    let dir = paths.caddy_storage();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    restrict_to_user(&dir, true)
}

/// Serializes `ensure` between the callers in this process (the supervisor and UI commands), so two
/// of them never generate two roots and leave a key next to the other one's certificate.
static ENSURE_LOCK: Mutex<()> = Mutex::new(());

/// Creates the root on first use. The key is written only after its folder is locked down. Both
/// files are written under temporary names and renamed into place, the certificate last: a root
/// counts as present only when both files exist, so an interrupted run is simply redone.
pub fn ensure(paths: &Paths) -> Result<(), String> {
    let _guard = ENSURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if paths.ca_cert().exists() && paths.ca_key().exists() {
        return Ok(());
    }
    let dir = paths.ca_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    restrict_to_user(&dir, true)?;
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let root = generate_root(format!("Konak Local CA {host}").trim())?;
    let (key_tmp, cert_tmp) = (dir.join("root.key.tmp"), dir.join("root.crt.tmp"));
    std::fs::write(&key_tmp, &root.key_pem).map_err(|e| e.to_string())?;
    restrict_to_user(&key_tmp, false)?;
    std::fs::write(&cert_tmp, &root.cert_pem).map_err(|e| e.to_string())?;
    std::fs::rename(&key_tmp, paths.ca_key()).map_err(|e| e.to_string())?;
    std::fs::rename(&cert_tmp, paths.ca_cert()).map_err(|e| e.to_string())?;
    crate::log(paths, "ca: root created");
    Ok(())
}

fn cert_der(paths: &Paths) -> Result<Vec<u8>, String> {
    let pem = std::fs::read_to_string(paths.ca_cert()).map_err(|e| format!("no root certificate: {e}"))?;
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())
}

/// SHA-256 fingerprint as colon-separated upper-case hex, the form phones and browsers display.
pub fn fingerprint(paths: &Paths) -> Result<String, String> {
    Ok(Sha256::digest(cert_der(paths)?).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

fn serial_hex(paths: &Paths) -> Result<String, String> {
    let der = cert_der(paths)?;
    let cert = x509_serial(&der).ok_or("cannot read the root's serial number")?;
    Ok(cert.iter().map(|b| format!("{b:02x}")).collect())
}

/// Serial number bytes from a DER certificate: Certificate > TBSCertificate > [0] version > INTEGER.
fn x509_serial(der: &[u8]) -> Option<&[u8]> {
    fn header(d: &[u8]) -> Option<(u8, usize, usize)> {
        let tag = *d.first()?;
        let first = *d.get(1)? as usize;
        if first < 0x80 {
            return Some((tag, 2, first));
        }
        let n = first & 0x7f;
        let len = d.get(2..2 + n)?.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        Some((tag, 2 + n, len))
    }
    let (_, h, _) = header(der)?; // Certificate SEQUENCE
    let tbs = &der[h..];
    let (_, h, _) = header(tbs)?; // TBSCertificate SEQUENCE
    let mut rest = &tbs[h..];
    let (tag, h, len) = header(rest)?;
    if tag == 0xa0 {
        rest = &rest[h + len..]; // skip the explicit version
    }
    let (tag, h, len) = header(rest)?;
    (tag == 0x02).then(|| &rest[h..h + len])
}

/// Adds the root to the current user's trusted roots (no admin). Windows shows a confirmation dialog.
pub fn trust_on_this_pc(paths: &Paths) -> Result<(), String> {
    let out = shell::run("certutil.exe", &["-user", "-addstore", "Root", &paths.ca_cert().to_string_lossy()]);
    if !out.ok {
        return Err(format!("certutil -addstore failed: {}", tail(&out.text, 3)));
    }
    Ok(())
}

/// Removes the root from the current user's trusted roots (uninstall). Windows may ask to confirm.
pub fn untrust_on_this_pc(paths: &Paths) -> Result<(), String> {
    let out = shell::run("certutil.exe", &["-user", "-delstore", "Root", &serial_hex(paths)?]);
    if !out.ok {
        return Err(format!("certutil -delstore failed: {}", tail(&out.text, 3)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_a_constrained_ca() {
        let root = generate_root("Konak Test CA").unwrap();
        assert!(root.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(root.key_pem.contains("PRIVATE KEY"));
        let b64: String = root.cert_pem.lines().filter(|l| !l.starts_with("-----")).collect();
        use base64::Engine;
        let der = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        // nameConstraints OID 2.5.29.30 and the permitted dNSName "local" ([2] IA5String).
        assert!(der.windows(5).any(|w| w == [0x06, 0x03, 0x55, 0x1d, 0x1e]));
        assert!(der.windows(7).any(|w| w == [0x82, 0x05, b'l', b'o', b'c', b'a', b'l']));
        // basicConstraints cA TRUE with pathLenConstraint 1, and EKU 2.5.29.37 with only serverAuth.
        assert!(der.windows(8).any(|w| w == [0x30, 0x06, 0x01, 0x01, 0xff, 0x02, 0x01, 0x01]));
        assert!(der.windows(5).any(|w| w == [0x06, 0x03, 0x55, 0x1d, 0x25]));
        let kp = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03]; // id-kp arc, 1.3.6.1.5.5.7.3
        let purposes: Vec<u8> = der.windows(10).filter(|w| w[..9] == kp).map(|w| w[9]).collect();
        assert_eq!(purposes, [1], "serverAuth and nothing else");
        let serial = x509_serial(&der).unwrap();
        assert_eq!(serial.len(), 16);
    }

    #[test]
    fn concurrent_ensure_leaves_one_matching_root() {
        let home = std::env::temp_dir().join(format!("konak-ca-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let paths = Paths { home: home.clone(), distro: String::new(), bin: home.clone(), media: home.clone(), photos: home.clone() };
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| ensure(&paths).unwrap());
            }
        });
        let key = KeyPair::from_pem(&std::fs::read_to_string(paths.ca_key()).unwrap()).unwrap();
        let der = cert_der(&paths).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(paths.ca_dir()).unwrap().flatten().map(|e| e.file_name()).collect();
        let _ = std::fs::remove_dir_all(&home);
        // The certificate carries the public half of the key on disk.
        assert!(der.windows(key.public_key_raw().len()).any(|w| w == key.public_key_raw()));
        assert_eq!(leftovers.len(), 2, "no temp files left: {leftovers:?}");
    }

    #[test]
    fn caddy_storage_is_user_only() {
        let home = std::env::temp_dir().join(format!("konak-caddy-acl-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: String::new(), bin: home.clone(), media: home.clone(), photos: home.clone() };
        prepare_caddy_storage(&paths).unwrap();
        let acl = shell::run("icacls.exe", &[&paths.caddy_storage().to_string_lossy()]).text;
        let sid = user_sid().unwrap();
        let _ = std::fs::remove_dir_all(&home);
        // One explicit entry (the current user, shown by name), nothing inherited.
        assert!(!acl.contains("(I)"), "{acl}");
        let entries = acl.lines().filter(|l| l.contains(":(")).count();
        assert_eq!(entries, 1, "{sid}: {acl}");
    }

    /// Manual check against the real Caddy (README "Checking the local CA"): writes a root (with the
    /// ACL) and a Caddyfile on 127.0.0.1 high ports into KONAK_CA_CHECK_DIR.
    #[test]
    #[ignore]
    fn write_root_and_caddyfile_for_manual_check() {
        use crate::services::{caddyfile, CaddyFiles, Route};
        let home = std::path::PathBuf::from(std::env::var("KONAK_CA_CHECK_DIR").expect("set KONAK_CA_CHECK_DIR"));
        let paths = Paths { home: home.clone(), distro: String::new(), bin: home.clone(), media: home.clone(), photos: home.clone() };
        ensure(&paths).unwrap();
        println!("fingerprint {}", fingerprint(&paths).unwrap());
        println!("serial {}", serial_hex(&paths).unwrap());
        let routes = [Route { id: "test".into(), name: "test".into(), port: 47199, https: true }];
        let (cert, key, site) = (paths.ca_cert(), paths.ca_key(), paths.site_dir());
        let files = CaddyFiles { storage: &paths.caddy_storage(), site: &site, ca: Some((&cert, &key)) };
        let c = caddyfile("127.0.0.1", &routes, &files).replace(":443 {", ":18443 {").replace(":80 {", ":18080 {");
        // A name outside .local, which the name constraint must make clients reject.
        let c = c + "https://outside.example:18443 {\n\tbind 127.0.0.1\n\ttls internal\n\trespond \"outside\"\n}\n";
        std::fs::write(home.join("Caddyfile"), c).unwrap();
    }
}
