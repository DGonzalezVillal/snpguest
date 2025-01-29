// SPDX-License-Identifier: Apache-2.0
// This file includes subcommands for verifying certificate chains and attestation reports. Submodules `certificate_chain` and `attestation` contain the verification logic for certificates and attestation reports, respectively.

use super::*;

use certs::{convert_path_to_cert, CertPaths};

use fetch::{get_processor_model, ProcType};

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use openssl::{ecdsa::EcdsaSig, sha::Sha384};
use sev::certs::snp::Chain;

#[derive(Subcommand)]
pub enum VerifyCmd {
    /// Verify the certificate chain.
    Certs(certificate_chain::Args),

    /// Verify the attestation report.
    Attestation(attestation::Args),
}

pub fn cmd(cmd: VerifyCmd, quiet: bool) -> Result<()> {
    match cmd {
        VerifyCmd::Certs(args) => certificate_chain::validate_cc(args, quiet),
        VerifyCmd::Attestation(args) => attestation::verify_attestation(args, quiet),
    }
}

// Find a certificate in specified directory according to its extension
pub fn find_cert_in_dir(dir: &Path, cert: &str) -> Result<PathBuf, anyhow::Error> {
    if dir.join(format!("{cert}.pem")).exists() {
        Ok(dir.join(format!("{cert}.pem")))
    } else if dir.join(format!("{cert}.der")).exists() {
        Ok(dir.join(format!("{cert}.der")))
    } else {
        return Err(anyhow::anyhow!("{cert} certificate not found in directory"));
    }
}

mod certificate_chain {
    use sev::certs::snp::Verifiable;

    use super::*;

    #[derive(Parser)]
    pub struct Args {
        /// Path to directory containing certificate chain."
        #[arg(value_name = "certs-dir", required = true)]
        pub certs_dir: PathBuf,
    }

    // Function to validate certificate chain
    pub fn validate_cc(args: Args, quiet: bool) -> Result<()> {
        let ark_path = find_cert_in_dir(&args.certs_dir, "ark")?;
        let (mut vek_type, mut sign_type): (&str, &str) = ("vcek", "ask");
        let (vek_path, ask_path) = match find_cert_in_dir(&args.certs_dir, "vlek") {
            Ok(vlek_path) => {
                (vek_type, sign_type) = ("vlek", "asvk");
                (vlek_path, find_cert_in_dir(&args.certs_dir, sign_type)?)
            }
            Err(_) => (
                find_cert_in_dir(&args.certs_dir, vek_type)?,
                find_cert_in_dir(&args.certs_dir, sign_type)?,
            ),
        };

        // Get a cert chain from directory
        let cert_chain: Chain = CertPaths {
            ark_path,
            ask_path,
            vek_path,
        }
        .try_into()?;

        let ark = cert_chain.ca.ark;
        let ask = cert_chain.ca.ask;
        let vek = cert_chain.vek;

        // Verify each signature and print result in console
        match (&ark, &ark).verify() {
            Ok(()) => {
                if !quiet {
                    println!("The AMD ARK was self-signed!");
                }
            }
            Err(e) => match e.kind() {
                ErrorKind::Other => return Err(anyhow::anyhow!("The AMD ARK is not self-signed!")),
                _ => {
                    return Err(anyhow::anyhow!(
                        "Failed to verify the ARK cerfificate: {:?}",
                        e
                    ))
                }
            },
        }

        match (&ark, &ask).verify() {
            Ok(()) => {
                if !quiet {
                    println!(
                        "The AMD {} was signed by the AMD ARK!",
                        sign_type.to_uppercase()
                    );
                }
            }
            Err(e) => match e.kind() {
                ErrorKind::Other => {
                    return Err(anyhow::anyhow!(
                        "The AMD {} was not signed by the AMD ARK!",
                        sign_type.to_uppercase()
                    ))
                }
                _ => return Err(anyhow::anyhow!("Failed to verify ASK certificate: {:?}", e)),
            },
        }

        match (&ask, &vek).verify() {
            Ok(()) => {
                if !quiet {
                    println!(
                        "The {} was signed by the AMD {}!",
                        vek_type.to_uppercase(),
                        sign_type.to_uppercase()
                    );
                }
            }
            Err(e) => match e.kind() {
                ErrorKind::Other => {
                    return Err(anyhow::anyhow!(
                        "The {} was not signed by the AMD {}!",
                        vek_type.to_uppercase(),
                        sign_type.to_uppercase(),
                    ))
                }
                _ => return Err(anyhow::anyhow!("Failed to verify VEK certificate: {:?}", e)),
            },
        }
        Ok(())
    }
}

mod attestation {
    use super::*;

    use asn1_rs::{oid, FromDer, Oid};

    use x509_parser::{self, certificate::X509Certificate, prelude::X509Extension, x509::X509Name};

    use sev::{
        certs::snp::Certificate,
        firmware::{guest::AttestationReport, host::CertType},
    };

    enum SnpOid {
        BootLoader,
        Tee,
        Snp,
        Ucode,
        HwId,
        Fmc,
    }

    // OID extensions for the VCEK, will be used to verify attestation report
    impl SnpOid {
        fn oid(&self) -> Oid {
            match self {
                SnpOid::BootLoader => oid!(1.3.6 .1 .4 .1 .3704 .1 .3 .1),
                SnpOid::Tee => oid!(1.3.6 .1 .4 .1 .3704 .1 .3 .2),
                SnpOid::Snp => oid!(1.3.6 .1 .4 .1 .3704 .1 .3 .3),
                SnpOid::Ucode => oid!(1.3.6 .1 .4 .1 .3704 .1 .3 .8),
                SnpOid::HwId => oid!(1.3.6 .1 .4 .1 .3704 .1 .4),
                SnpOid::Fmc => oid!(1.3.6 .1 .4 .1 .3704 .1 .3 .9),
            }
        }
    }

    #[derive(Parser)]
    pub struct Args {
        /// Path to directory containing VCEK.
        #[arg(value_name = "certs-dir", required = true)]
        pub certs_dir: PathBuf,

        /// Path to attestation report to use for validation.
        #[arg(value_name = "att-report-path", required = true)]
        pub att_report_path: PathBuf,

        /// Specify the processor model to verify the attestation report.
        #[arg(short, long, value_name = "processor-model", ignore_case = true)]
        pub processor_model: Option<ProcType>,

        /// Run the TCB Verification Exclusively.
        #[arg(short, long, conflicts_with = "signature")]
        pub tcb: bool,

        /// Run the Signature Verification Exclusively.
        #[arg(short, long, conflicts_with = "tcb")]
        pub signature: bool,
    }

    fn verify_attestation_signature(
        vcek: Certificate,
        att_report: AttestationReport,
        quiet: bool,
    ) -> Result<()> {
        let vek_pubkey = vcek
            .public_key()
            .context("Failed to get the public key from the VEK.")?
            .ec_key()
            .context("Failed to convert VEK public key into ECkey.")?;

        // Get the attestation report signature
        let ar_signature = EcdsaSig::try_from(&att_report.signature)
            .context("Failed to get ECDSA Signature from attestation report.")?;
        let mut report_bytes = Vec::new();
        att_report.write_bytes(&mut report_bytes)?;
        let signed_bytes = &report_bytes[0x0..0x2A0];

        let mut hasher: Sha384 = Sha384::new();

        hasher.update(signed_bytes);

        let base_message_digest: [u8; 48] = hasher.finish();

        // Verify signature
        if ar_signature
            .verify(base_message_digest.as_ref(), vek_pubkey.as_ref())
            .context("Failed to verify attestation report signature with VEK public key.")?
        {
            if !quiet {
                println!("VEK signed the Attestation Report!");
            }
        } else {
            return Err(anyhow::anyhow!("VEK did NOT sign the Attestation Report!"));
        }

        Ok(())
    }

    // Check the cert extension byte to value
    fn check_cert_bytes(ext: &X509Extension, val: &[u8]) -> bool {
        match ext.value[0] {
            // Integer
            0x2 => {
                if ext.value[1] != 0x1 && ext.value[1] != 0x2 {
                    panic!("Invalid octet length encountered!");
                } else if let Some(byte_value) = ext.value.last() {
                    byte_value == &val[0]
                } else {
                    false
                }
            }
            // Octet String
            0x4 => {
                if ext.value[1] != 0x40 {
                    panic!("Invalid octet length encountered!");
                } else if ext.value[2..].len() != 0x40 {
                    panic!("Invalid size of bytes encountered!");
                } else if val.len() != 0x40 {
                    panic!("Invalid certificate harward id length encountered!")
                }

                &ext.value[2..] == val
            }
            // Legacy and others.
            _ => {
                // Keep around for a bit for old VCEK without x509 DER encoding.
                if ext.value.len() == 0x40 && val.len() == 0x40 {
                    ext.value == val
                } else {
                    panic!("Invalid type encountered!");
                }
            }
        }
    }

    fn parse_common_name(field: &X509Name<'_>) -> Result<CertType> {
        if let Some(val) = field
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
        {
            match val.to_lowercase() {
                x if x.contains("ark") => Ok(CertType::ARK),
                x if x.contains("ask") | x.contains("sev") => Ok(CertType::ASK),
                x if x.contains("vcek") => Ok(CertType::VCEK),
                x if x.contains("vlek") => Ok(CertType::VLEK),
                x if x.contains("crl") => Ok(CertType::CRL),
                _ => Err(anyhow::anyhow!("Unknown certificate type encountered!")),
            }
        } else {
            Err(anyhow::anyhow!(
                "Certificate Subject Common Name is Unknown!"
            ))
        }
    }

    fn verify_attestation_tcb(
        vcek: Certificate,
        att_report: AttestationReport,
        proc_model: ProcType,
        quiet: bool,
    ) -> Result<()> {
        let vek_der = vcek.to_der().context("Could not convert VEK to der.")?;
        let (_, vek_x509) = X509Certificate::from_der(&vek_der)
            .context("Could not create X509Certificate from der")?;

        // Collect extensions from VEK
        let extensions: std::collections::HashMap<Oid, &X509Extension> = vek_x509
            .extensions_map()
            .context("Failed getting VEK oids.")?;

        let common_name: CertType = parse_common_name(vek_x509.subject())?;

        // Compare bootloaders
        if let Some(cert_bl) = extensions.get(&SnpOid::BootLoader.oid()) {
            if !check_cert_bytes(cert_bl, &att_report.reported_tcb.bootloader.to_le_bytes()) {
                return Err(anyhow::anyhow!(
                    "Report TCB Boot Loader and Certificate Boot Loader mismatch encountered."
                ));
            }
            if !quiet {
                println!(
                    "Reported TCB Boot Loader from certificate matches the attestation report."
                );
            }
        }

        // Compare TEE information
        if let Some(cert_tee) = extensions.get(&SnpOid::Tee.oid()) {
            if !check_cert_bytes(cert_tee, &att_report.reported_tcb.tee.to_le_bytes()) {
                return Err(anyhow::anyhow!(
                    "Report TCB TEE and Certificate TEE mismatch encountered."
                ));
            }
            if !quiet {
                println!("Reported TCB TEE from certificate matches the attestation report.");
            }
        }

        // Compare SNP information
        if let Some(cert_snp) = extensions.get(&SnpOid::Snp.oid()) {
            if !check_cert_bytes(cert_snp, &att_report.reported_tcb.snp.to_le_bytes()) {
                return Err(anyhow::anyhow!(
                    "Report TCB SNP and Certificate SNP mismatch encountered."
                ));
            }
            if !quiet {
                println!("Reported TCB SNP from certificate matches the attestation report.");
            }
        }

        // Compare Microcode information
        if let Some(cert_ucode) = extensions.get(&SnpOid::Ucode.oid()) {
            if !check_cert_bytes(cert_ucode, &att_report.reported_tcb.microcode.to_le_bytes()) {
                return Err(anyhow::anyhow!(
                    "Report TCB Microcode and Certificate Microcode mismatch encountered."
                ));
            }
            if !quiet {
                println!("Reported TCB Microcode from certificate matches the attestation report.");
            }
        }

        // Compare HWID information only on VCEK
        if common_name == CertType::VCEK {
            if let Some(cert_hwid) = extensions.get(&SnpOid::HwId.oid()) {
                if !check_cert_bytes(cert_hwid, att_report.chip_id.as_slice()) {
                    return Err(anyhow::anyhow!(
                        "Report TCB ID and Certificate ID mismatch encountered."
                    ));
                }
                if !quiet {
                    println!("Chip ID from certificate matches the attestation report.");
                }
            }
        }

        if proc_model == ProcType::Turin {
            if att_report.version < 3 {
                return Err(anyhow::anyhow!(
                    "Turin Attestation is not supported in version 2 of the report."
                ));
            }
            if let Some(cert_fmc) = extensions.get(&SnpOid::Fmc.oid()) {
                if !check_cert_bytes(
                    cert_fmc,
                    &att_report.reported_tcb.fmc.unwrap().to_le_bytes(),
                ) {
                    return Err(anyhow::anyhow!(
                        "Report TCB FMC and Certificate FMC mismatch encountered."
                    ));
                }
                if !quiet {
                    println!("Reported TCB FMC from certificate matches the attestation report.");
                }
            }
        }

        Ok(())
    }

    pub fn verify_attestation(args: Args, quiet: bool) -> Result<()> {
        // Get attestation report
        let att_report = if !args.att_report_path.exists() {
            return Err(anyhow::anyhow!("No attestation report was found. Provide an attestation report to request VEK from the KDS."));
        } else {
            report::read_report(args.att_report_path.clone())
                .context("Could not open attestation report")?
        };

        let proc_model = if let Some(proc_model) = args.processor_model {
            proc_model
        } else {
            let att_report = report::read_report(args.att_report_path.clone())
                .context("Could not open attestation report")?;
            get_processor_model(att_report)?
        };

        // Get VEK and its public key.
        let (vek_path, vek_type) = match find_cert_in_dir(&args.certs_dir, "vlek") {
            Ok(vlek_path) => (vlek_path, "vlek"),
            Err(_) => (find_cert_in_dir(&args.certs_dir, "vcek")?, "vcek"),
        };

        // Get VEK and grab its public key
        let vek = convert_path_to_cert(&vek_path, vek_type)?;

        if args.tcb || args.signature {
            if args.tcb {
                verify_attestation_tcb(vek.clone(), att_report, proc_model, quiet)?;
            }
            if args.signature {
                verify_attestation_signature(vek, att_report, quiet)?;
            }
        } else {
            verify_attestation_tcb(vek.clone(), att_report, proc_model, quiet)?;
            verify_attestation_signature(vek, att_report, quiet)?;
        }

        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use x509_parser::{self, certificate::X509Certificate};

        /// Important note that this is NOT a valid certificate,
        /// and the signature will NOT match at all.
        fn cert_and_hw_id_legacy() -> ([u8; 1361], [u8; 64]) {
            (
                [
                    0x30, 0x82, 0x05, 0x4d, 0x30, 0x82, 0x02, 0xfc, 0xa0, 0x03, 0x02, 0x01, 0x02,
                    0x02, 0x01, 0x00, 0x30, 0x46, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d,
                    0x01, 0x01, 0x0a, 0x30, 0x39, 0xa0, 0x0f, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86,
                    0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa1, 0x1c, 0x30, 0x1a,
                    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08, 0x30, 0x0d,
                    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00,
                    0xa2, 0x03, 0x02, 0x01, 0x30, 0xa3, 0x03, 0x02, 0x01, 0x01, 0x30, 0x7b, 0x31,
                    0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x0b, 0x0c, 0x0b, 0x45, 0x6e, 0x67,
                    0x69, 0x6e, 0x65, 0x65, 0x72, 0x69, 0x6e, 0x67, 0x31, 0x0b, 0x30, 0x09, 0x06,
                    0x03, 0x55, 0x04, 0x06, 0x13, 0x02, 0x55, 0x53, 0x31, 0x14, 0x30, 0x12, 0x06,
                    0x03, 0x55, 0x04, 0x07, 0x0c, 0x0b, 0x53, 0x61, 0x6e, 0x74, 0x61, 0x20, 0x43,
                    0x6c, 0x61, 0x72, 0x61, 0x31, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x04, 0x08,
                    0x0c, 0x02, 0x43, 0x41, 0x31, 0x1f, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x04, 0x0a,
                    0x0c, 0x16, 0x41, 0x64, 0x76, 0x61, 0x6e, 0x63, 0x65, 0x64, 0x20, 0x4d, 0x69,
                    0x63, 0x72, 0x6f, 0x20, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x73, 0x31, 0x12,
                    0x30, 0x10, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x09, 0x53, 0x45, 0x56, 0x2d,
                    0x4d, 0x69, 0x6c, 0x61, 0x6e, 0x30, 0x1e, 0x17, 0x0d, 0x32, 0x33, 0x30, 0x32,
                    0x30, 0x33, 0x32, 0x32, 0x34, 0x31, 0x35, 0x35, 0x5a, 0x17, 0x0d, 0x33, 0x30,
                    0x30, 0x32, 0x30, 0x33, 0x32, 0x32, 0x34, 0x31, 0x35, 0x35, 0x5a, 0x30, 0x7a,
                    0x31, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x0b, 0x0c, 0x0b, 0x45, 0x6e,
                    0x67, 0x69, 0x6e, 0x65, 0x65, 0x72, 0x69, 0x6e, 0x67, 0x31, 0x0b, 0x30, 0x09,
                    0x06, 0x03, 0x55, 0x04, 0x06, 0x13, 0x02, 0x55, 0x53, 0x31, 0x14, 0x30, 0x12,
                    0x06, 0x03, 0x55, 0x04, 0x07, 0x0c, 0x0b, 0x53, 0x61, 0x6e, 0x74, 0x61, 0x20,
                    0x43, 0x6c, 0x61, 0x72, 0x61, 0x31, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x04,
                    0x08, 0x0c, 0x02, 0x43, 0x41, 0x31, 0x1f, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x04,
                    0x0a, 0x0c, 0x16, 0x41, 0x64, 0x76, 0x61, 0x6e, 0x63, 0x65, 0x64, 0x20, 0x4d,
                    0x69, 0x63, 0x72, 0x6f, 0x20, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x73, 0x31,
                    0x11, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x08, 0x53, 0x45, 0x56,
                    0x2d, 0x56, 0x43, 0x45, 0x4b, 0x30, 0x76, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86,
                    0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22, 0x03,
                    0x62, 0x00, 0x04, 0x70, 0x5b, 0xf7, 0x2b, 0xaa, 0xb4, 0x71, 0xed, 0xbe, 0x34,
                    0x4b, 0xbf, 0xaf, 0x15, 0xa3, 0xa2, 0x94, 0x1b, 0xc1, 0x54, 0x54, 0x1c, 0xec,
                    0x54, 0x25, 0xac, 0xa8, 0x27, 0xb2, 0x83, 0xd3, 0x9d, 0xdb, 0x68, 0xf9, 0xea,
                    0x6b, 0x37, 0x64, 0x6c, 0x5c, 0x92, 0x05, 0x7f, 0x5a, 0x9f, 0x10, 0xe9, 0x07,
                    0xfb, 0x33, 0x66, 0x51, 0xe0, 0x91, 0xc2, 0x9f, 0x4f, 0x48, 0xbd, 0x4d, 0x44,
                    0x14, 0xb4, 0x89, 0xdd, 0x5b, 0x8e, 0xb0, 0x69, 0x60, 0x75, 0x75, 0x84, 0x2e,
                    0x9d, 0x93, 0x1a, 0x7d, 0x95, 0xad, 0xd3, 0xb7, 0xa4, 0x8c, 0x78, 0xad, 0xf5,
                    0x5c, 0x9f, 0x56, 0x2e, 0x39, 0x83, 0xdc, 0xc9, 0xa3, 0x82, 0x01, 0x17, 0x30,
                    0x82, 0x01, 0x13, 0x30, 0x10, 0x06, 0x09, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c,
                    0x78, 0x01, 0x01, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x17, 0x06, 0x09, 0x2b,
                    0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x02, 0x04, 0x0a, 0x16, 0x08, 0x4d,
                    0x69, 0x6c, 0x61, 0x6e, 0x2d, 0x42, 0x30, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06,
                    0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x01, 0x04, 0x03, 0x02, 0x01, 0x03,
                    0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03,
                    0x02, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01,
                    0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x04, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30,
                    0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x05,
                    0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04,
                    0x01, 0x9c, 0x78, 0x01, 0x03, 0x06, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11,
                    0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x07, 0x04,
                    0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01,
                    0x9c, 0x78, 0x01, 0x03, 0x03, 0x04, 0x03, 0x02, 0x01, 0x0a, 0x30, 0x12, 0x06,
                    0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x08, 0x04, 0x04,
                    0x02, 0x02, 0x00, 0xa9, 0x30, 0x4d, 0x06, 0x09, 0x2b, 0x06, 0x01, 0x04, 0x01,
                    0x9c, 0x78, 0x01, 0x04, 0x04, 0x40, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
                    0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
                    0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
                    0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
                    0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a,
                    0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x30, 0x46, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86,
                    0xf7, 0x0d, 0x01, 0x01, 0x0a, 0x30, 0x39, 0xa0, 0x0f, 0x30, 0x0d, 0x06, 0x09,
                    0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa1, 0x1c,
                    0x30, 0x1a, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08,
                    0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02,
                    0x05, 0x00, 0xa2, 0x03, 0x02, 0x01, 0x30, 0xa3, 0x03, 0x02, 0x01, 0x01, 0x03,
                    0x82, 0x02, 0x01, 0x00, 0x31, 0xf0, 0x71, 0xa1, 0xc2, 0x35, 0xee, 0x9f, 0xb5,
                    0x87, 0x0d, 0x2e, 0xe9, 0xa2, 0x28, 0x47, 0x1d, 0xb2, 0x5b, 0xfd, 0x74, 0x58,
                    0xa2, 0xc9, 0x85, 0xac, 0x4e, 0xad, 0x59, 0x9a, 0xba, 0x93, 0xfb, 0xc0, 0x12,
                    0xc3, 0x4b, 0xf5, 0x10, 0x5a, 0xd9, 0xbe, 0x75, 0x1f, 0xeb, 0x5f, 0xed, 0x86,
                    0x3f, 0xe7, 0xca, 0xb3, 0xd7, 0x98, 0xa8, 0x97, 0x6e, 0xc8, 0x9e, 0xbf, 0x8f,
                    0x2e, 0x0f, 0xab, 0xbd, 0xa6, 0xe6, 0xac, 0xe8, 0x42, 0x2d, 0xd3, 0x1c, 0x7f,
                    0x4e, 0xd1, 0x07, 0xd8, 0x9b, 0x3a, 0x9d, 0xdc, 0x59, 0xf5, 0x97, 0x59, 0x96,
                    0xbd, 0xc6, 0xc1, 0x70, 0x03, 0x4c, 0xc0, 0xce, 0x40, 0xd9, 0xec, 0x41, 0x27,
                    0xe5, 0xf2, 0xa5, 0xf5, 0x6f, 0x40, 0x8c, 0xf9, 0x31, 0x62, 0x0d, 0xc8, 0xd1,
                    0xf9, 0x5b, 0x2a, 0x8f, 0x20, 0xc5, 0x77, 0xd8, 0x47, 0x68, 0xc9, 0xaa, 0xc9,
                    0xd9, 0x3e, 0x43, 0x69, 0x69, 0xae, 0xce, 0xcc, 0x58, 0xc7, 0x3f, 0xdd, 0x2a,
                    0xa3, 0x2c, 0x89, 0x8f, 0x1f, 0x57, 0xe1, 0x0b, 0x50, 0x81, 0x0b, 0x61, 0xb2,
                    0x97, 0x4f, 0x33, 0xb1, 0xd9, 0x20, 0x21, 0x5f, 0x1f, 0xac, 0xa0, 0x11, 0xc6,
                    0xb5, 0xcd, 0xa7, 0x50, 0xe6, 0xe4, 0x83, 0x54, 0x16, 0xe9, 0x03, 0x92, 0x1f,
                    0x4e, 0xb1, 0x58, 0xe8, 0xe6, 0xb0, 0x66, 0xf0, 0x00, 0x9f, 0x42, 0x20, 0xb3,
                    0x0e, 0x8d, 0xd2, 0x0b, 0x3b, 0x65, 0xfc, 0x6b, 0x4c, 0x69, 0x63, 0x10, 0xa4,
                    0xb5, 0x92, 0xaa, 0x16, 0xfb, 0xf3, 0x6b, 0x4d, 0xf7, 0x7c, 0x69, 0x63, 0x51,
                    0x0e, 0x5c, 0x5b, 0x5f, 0x66, 0x56, 0x62, 0x8e, 0x56, 0x69, 0xc0, 0x97, 0xa4,
                    0x16, 0x68, 0xc0, 0xe6, 0xb1, 0xbe, 0x9f, 0x7b, 0x28, 0x0b, 0x94, 0x71, 0xc4,
                    0x70, 0x82, 0xbb, 0x0b, 0xd4, 0x8b, 0x1a, 0xd9, 0x11, 0x78, 0x2c, 0x0d, 0xe2,
                    0xaf, 0x92, 0xd5, 0x88, 0xbf, 0x10, 0xf1, 0x0b, 0x6c, 0x05, 0x16, 0x2f, 0xa0,
                    0xef, 0x24, 0xf2, 0xf6, 0x90, 0x0a, 0x88, 0xca, 0x76, 0xc2, 0xb0, 0xf0, 0xff,
                    0x52, 0x9a, 0x10, 0xc4, 0x4e, 0xed, 0xab, 0x24, 0x30, 0x87, 0x9f, 0xa4, 0x63,
                    0x21, 0x1a, 0xb4, 0xc2, 0xc6, 0x8d, 0x13, 0x02, 0xb1, 0xab, 0x5e, 0x2a, 0x45,
                    0xd1, 0x22, 0x6e, 0x7a, 0x93, 0xca, 0xf1, 0x87, 0x5d, 0x21, 0x0c, 0xef, 0x84,
                    0xac, 0x54, 0xfd, 0x01, 0xe6, 0x77, 0x0a, 0xf1, 0x33, 0x2d, 0xeb, 0x8a, 0x45,
                    0xb0, 0xdf, 0x80, 0x56, 0x2d, 0xaf, 0xee, 0x25, 0x29, 0x55, 0xe2, 0xc9, 0xfb,
                    0xce, 0x08, 0x4f, 0x39, 0x47, 0x3f, 0x02, 0x41, 0xab, 0x71, 0xac, 0xd2, 0xd1,
                    0xf1, 0xb6, 0x4d, 0xc6, 0xe4, 0x1f, 0xf9, 0x9f, 0x11, 0x56, 0x28, 0xe4, 0xef,
                    0x99, 0x70, 0x4c, 0x50, 0x07, 0xef, 0x0d, 0xb1, 0xea, 0xc1, 0xad, 0x3c, 0xb0,
                    0xcf, 0xe3, 0x2f, 0x41, 0xcf, 0x3b, 0x1b, 0x1a, 0xfb, 0x61, 0xca, 0x12, 0x42,
                    0xaf, 0x27, 0x73, 0x91, 0x37, 0x9e, 0xac, 0xd3, 0x0e, 0xe9, 0xb3, 0x18, 0xbd,
                    0xf8, 0xbc, 0x39, 0xf8, 0xcb, 0xbb, 0x6b, 0x56, 0x62, 0x1d, 0x22, 0x8c, 0x0a,
                    0x76, 0x44, 0xfb, 0x13, 0x71, 0xb9, 0xff, 0xf9, 0xc3, 0x42, 0x89, 0x89, 0x6e,
                    0x63, 0x21, 0x6f, 0x47, 0xdd, 0x56, 0x72, 0xd4, 0x59, 0x14, 0x70, 0x98, 0x88,
                    0xf7, 0x52, 0xcb, 0x9d, 0x9a, 0xc3, 0x1c, 0x3d, 0x9b, 0xda, 0xad, 0xec, 0xc4,
                    0x78, 0xa7, 0x74, 0xa0, 0x01, 0xab, 0x35, 0x46, 0xa0, 0xba, 0xb1, 0x77, 0xff,
                    0x88, 0x71, 0xdc, 0x0a, 0xc3, 0x70, 0x12, 0xb3, 0x18, 0x55, 0x01, 0x3d, 0x83,
                    0x28, 0xf8, 0xd7, 0x88, 0x43, 0xfa, 0xc7, 0xa2, 0x3f, 0xcd, 0x2b, 0xcb, 0xcf,
                    0x7f, 0x57, 0x6d, 0x3a, 0xc8, 0x14, 0x4e, 0x88, 0x8d,
                ],
                [
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
                    0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
                    0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26,
                    0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33,
                    0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
                ],
            )
        }

        /// Important note that this is NOT a valid certificate,
        /// and the signature will NOT match at all.
        fn cert_and_hw_id() -> ([u8; 1362], [u8; 64]) {
            (
                [
                    0x30, 0x82, 0x05, 0x4e, 0x30, 0x82, 0x02, 0xfd, 0xa0, 0x03, 0x02, 0x01, 0x02,
                    0x02, 0x01, 0x00, 0x30, 0x46, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d,
                    0x01, 0x01, 0x0a, 0x30, 0x39, 0xa0, 0x0f, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86,
                    0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa1, 0x1c, 0x30, 0x1a,
                    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08, 0x30, 0x0d,
                    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00,
                    0xa2, 0x03, 0x02, 0x01, 0x30, 0xa3, 0x03, 0x02, 0x01, 0x01, 0x30, 0x7b, 0x31,
                    0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x0b, 0x0c, 0x0b, 0x45, 0x6e, 0x67,
                    0x69, 0x6e, 0x65, 0x65, 0x72, 0x69, 0x6e, 0x67, 0x31, 0x0b, 0x30, 0x09, 0x06,
                    0x03, 0x55, 0x04, 0x06, 0x13, 0x02, 0x55, 0x53, 0x31, 0x14, 0x30, 0x12, 0x06,
                    0x03, 0x55, 0x04, 0x07, 0x0c, 0x0b, 0x53, 0x61, 0x6e, 0x74, 0x61, 0x20, 0x43,
                    0x6c, 0x61, 0x72, 0x61, 0x31, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x04, 0x08,
                    0x0c, 0x02, 0x43, 0x41, 0x31, 0x1f, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x04, 0x0a,
                    0x0c, 0x16, 0x41, 0x64, 0x76, 0x61, 0x6e, 0x63, 0x65, 0x64, 0x20, 0x4d, 0x69,
                    0x63, 0x72, 0x6f, 0x20, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x73, 0x31, 0x12,
                    0x30, 0x10, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x09, 0x53, 0x45, 0x56, 0x2d,
                    0x4d, 0x69, 0x6c, 0x61, 0x6e, 0x30, 0x1e, 0x17, 0x0d, 0x32, 0x33, 0x30, 0x38,
                    0x31, 0x37, 0x31, 0x34, 0x32, 0x37, 0x30, 0x39, 0x5a, 0x17, 0x0d, 0x33, 0x30,
                    0x30, 0x38, 0x31, 0x37, 0x31, 0x34, 0x32, 0x37, 0x30, 0x39, 0x5a, 0x30, 0x7a,
                    0x31, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x0b, 0x0c, 0x0b, 0x45, 0x6e,
                    0x67, 0x69, 0x6e, 0x65, 0x65, 0x72, 0x69, 0x6e, 0x67, 0x31, 0x0b, 0x30, 0x09,
                    0x06, 0x03, 0x55, 0x04, 0x06, 0x13, 0x02, 0x55, 0x53, 0x31, 0x14, 0x30, 0x12,
                    0x06, 0x03, 0x55, 0x04, 0x07, 0x0c, 0x0b, 0x53, 0x61, 0x6e, 0x74, 0x61, 0x20,
                    0x43, 0x6c, 0x61, 0x72, 0x61, 0x31, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x04,
                    0x08, 0x0c, 0x02, 0x43, 0x41, 0x31, 0x1f, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x04,
                    0x0a, 0x0c, 0x16, 0x41, 0x64, 0x76, 0x61, 0x6e, 0x63, 0x65, 0x64, 0x20, 0x4d,
                    0x69, 0x63, 0x72, 0x6f, 0x20, 0x44, 0x65, 0x76, 0x69, 0x63, 0x65, 0x73, 0x31,
                    0x11, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x08, 0x53, 0x45, 0x56,
                    0x2d, 0x56, 0x43, 0x45, 0x4b, 0x30, 0x76, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86,
                    0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22, 0x03,
                    0x62, 0x00, 0x04, 0x07, 0x79, 0x5c, 0xaa, 0x60, 0x2f, 0x16, 0x5e, 0x8d, 0x37,
                    0x46, 0x93, 0x87, 0xc5, 0x06, 0x4a, 0x52, 0x46, 0xc9, 0x72, 0x0b, 0xdb, 0x7a,
                    0xd2, 0x15, 0xb2, 0xc6, 0x61, 0x3c, 0x6f, 0x9b, 0x1e, 0xd4, 0x61, 0x48, 0xee,
                    0xbd, 0xdd, 0xef, 0x56, 0xc3, 0xb6, 0x40, 0xdf, 0xd0, 0x5e, 0xbb, 0x3c, 0x0c,
                    0x77, 0x2e, 0xea, 0x5a, 0xb0, 0xa9, 0x4b, 0x2e, 0x9a, 0x85, 0x92, 0x08, 0x55,
                    0x7c, 0x23, 0xc3, 0x2a, 0xe1, 0xac, 0xb0, 0x2f, 0x3d, 0x59, 0x15, 0xe9, 0xbd,
                    0x2e, 0x64, 0xb4, 0x37, 0x73, 0xb8, 0x04, 0xd5, 0xd5, 0x1b, 0x11, 0x5e, 0x60,
                    0x1a, 0xc1, 0xf3, 0x86, 0x9d, 0x3e, 0x32, 0xe2, 0xa3, 0x82, 0x01, 0x18, 0x30,
                    0x82, 0x01, 0x14, 0x30, 0x10, 0x06, 0x09, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c,
                    0x78, 0x01, 0x01, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x17, 0x06, 0x09, 0x2b,
                    0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x02, 0x04, 0x0a, 0x16, 0x08, 0x4d,
                    0x69, 0x6c, 0x61, 0x6e, 0x2d, 0x42, 0x30, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06,
                    0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x01, 0x04, 0x03, 0x02, 0x01, 0x00,
                    0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03,
                    0x02, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01,
                    0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x04, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30,
                    0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x05,
                    0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04,
                    0x01, 0x9c, 0x78, 0x01, 0x03, 0x06, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11,
                    0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x07, 0x04,
                    0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01,
                    0x9c, 0x78, 0x01, 0x03, 0x03, 0x04, 0x03, 0x02, 0x01, 0x00, 0x30, 0x11, 0x06,
                    0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c, 0x78, 0x01, 0x03, 0x08, 0x04, 0x03,
                    0x02, 0x01, 0x1e, 0x30, 0x4f, 0x06, 0x09, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x9c,
                    0x78, 0x01, 0x04, 0x04, 0x42, 0x04, 0x40, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
                    0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12,
                    0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
                    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
                    0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
                    0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x30, 0x46, 0x06, 0x09, 0x2a, 0x86, 0x48,
                    0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a, 0x30, 0x39, 0xa0, 0x0f, 0x30, 0x0d, 0x06,
                    0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa1,
                    0x1c, 0x30, 0x1a, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01,
                    0x08, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                    0x02, 0x05, 0x00, 0xa2, 0x03, 0x02, 0x01, 0x30, 0xa3, 0x03, 0x02, 0x01, 0x01,
                    0x03, 0x82, 0x02, 0x01, 0x00, 0x12, 0x41, 0x24, 0x4a, 0xf3, 0xf8, 0xfb, 0x0f,
                    0x70, 0x33, 0x9a, 0x0e, 0x36, 0x9e, 0xf5, 0x89, 0xad, 0x85, 0x6b, 0xed, 0xd1,
                    0x25, 0x2d, 0x23, 0x89, 0x16, 0x80, 0xcb, 0xee, 0xbd, 0x70, 0x97, 0x92, 0x24,
                    0x76, 0x0b, 0xf9, 0x15, 0x9e, 0x8e, 0x4c, 0xb4, 0x9d, 0x61, 0x9d, 0x3d, 0xfe,
                    0x3a, 0xf3, 0x36, 0xb4, 0xc8, 0xb7, 0x56, 0xad, 0x1a, 0x1f, 0x35, 0xf5, 0x36,
                    0xf9, 0xb5, 0xed, 0x8f, 0x95, 0x0d, 0x37, 0x0f, 0xa8, 0x89, 0xba, 0x1c, 0x96,
                    0x91, 0x97, 0x62, 0x4f, 0xc7, 0x93, 0x87, 0x6d, 0x23, 0xdc, 0xc0, 0xbb, 0xcd,
                    0x17, 0x38, 0xae, 0xbd, 0x0d, 0xc4, 0xcc, 0xa4, 0x3f, 0xc8, 0x7d, 0x0d, 0x0b,
                    0x5c, 0xf1, 0xba, 0x9b, 0x20, 0x29, 0x95, 0xb0, 0x96, 0x02, 0x4d, 0x9d, 0xcd,
                    0x82, 0x0a, 0x60, 0x92, 0x51, 0xa1, 0x3c, 0x69, 0xec, 0x27, 0x81, 0x8e, 0x28,
                    0xc7, 0x4e, 0x34, 0xbb, 0x9f, 0xb0, 0x49, 0xc7, 0x6e, 0xe6, 0xb7, 0x6b, 0x1f,
                    0x91, 0x20, 0x0a, 0x80, 0xd2, 0x9f, 0x67, 0x24, 0xe0, 0x75, 0x40, 0x9b, 0x4a,
                    0xdd, 0xeb, 0xab, 0x34, 0x5f, 0x59, 0x3d, 0x3b, 0x06, 0xf0, 0x4d, 0x7d, 0xf9,
                    0x26, 0xeb, 0x35, 0xcb, 0x08, 0x35, 0x7b, 0xbf, 0x02, 0x4e, 0xa5, 0x50, 0xf8,
                    0x91, 0xf3, 0x60, 0xed, 0x80, 0x0d, 0xe1, 0x7e, 0x2b, 0x86, 0x75, 0x3d, 0x0c,
                    0x83, 0xea, 0x64, 0x50, 0x6c, 0xbd, 0xe2, 0x17, 0x6e, 0x45, 0xaa, 0x10, 0xe8,
                    0x84, 0xcc, 0xa1, 0x06, 0xb6, 0x8b, 0xa5, 0x96, 0xb0, 0x83, 0xba, 0x61, 0xe6,
                    0xa4, 0x14, 0xd3, 0x26, 0xf3, 0x19, 0x31, 0xbe, 0x40, 0x2a, 0x18, 0x53, 0x58,
                    0x75, 0x1d, 0x46, 0xe2, 0xfe, 0x47, 0xa3, 0xa9, 0x39, 0x68, 0xee, 0x37, 0x8f,
                    0x57, 0xe6, 0x12, 0x92, 0x34, 0xa6, 0x0a, 0x51, 0xcb, 0x4c, 0xce, 0x54, 0xe2,
                    0xbe, 0x8b, 0x8c, 0x02, 0xe5, 0x3c, 0x3a, 0x7b, 0x7f, 0x7b, 0x3b, 0x80, 0x44,
                    0x98, 0x9c, 0x52, 0x1d, 0x29, 0x42, 0xce, 0x9f, 0x95, 0xc5, 0x79, 0xbe, 0xd8,
                    0x06, 0x71, 0xff, 0xa2, 0x0a, 0xe2, 0x21, 0xa9, 0x59, 0xda, 0xac, 0x05, 0xe8,
                    0x2e, 0xa5, 0x1f, 0x01, 0xaf, 0xae, 0xc6, 0x90, 0xbb, 0x5d, 0x7b, 0xa9, 0x84,
                    0xff, 0x1c, 0x11, 0x78, 0x07, 0x89, 0x0a, 0x09, 0x4f, 0xc8, 0x4c, 0xb1, 0x7e,
                    0x68, 0x12, 0xa6, 0x3d, 0xae, 0x6b, 0x69, 0x8d, 0xc9, 0x03, 0x5f, 0x4d, 0x45,
                    0x47, 0xde, 0xf0, 0xa5, 0x1a, 0x19, 0x97, 0x37, 0x0e, 0xe8, 0x8a, 0xd2, 0x30,
                    0x07, 0xbf, 0xb4, 0x09, 0x80, 0x93, 0xa4, 0x91, 0x28, 0x40, 0xe3, 0x2c, 0xf3,
                    0x46, 0xf0, 0x22, 0xb3, 0xb7, 0xc5, 0x92, 0x69, 0x7a, 0x4d, 0xdb, 0xf7, 0x67,
                    0x97, 0x6f, 0x83, 0xcf, 0x5d, 0x29, 0x8b, 0x55, 0x72, 0xd3, 0xa2, 0xcb, 0x65,
                    0x21, 0x76, 0x84, 0xed, 0x75, 0xd5, 0xf3, 0x74, 0xff, 0xc1, 0x1a, 0x8d, 0x65,
                    0xac, 0x4f, 0xb0, 0x8c, 0x87, 0xae, 0x6a, 0xf0, 0xf9, 0x56, 0x23, 0xfc, 0x29,
                    0x5a, 0x1c, 0xd4, 0x12, 0xf9, 0x79, 0x66, 0x97, 0xad, 0x95, 0xc1, 0xa9, 0x0e,
                    0xf3, 0x2b, 0x94, 0x17, 0xc3, 0xfd, 0x51, 0x1f, 0x94, 0x35, 0xad, 0xa7, 0xf9,
                    0x61, 0x57, 0xf3, 0x67, 0x53, 0x17, 0xc7, 0xee, 0x1f, 0x54, 0x11, 0x1a, 0xd4,
                    0xc9, 0x33, 0x4b, 0x3a, 0x71, 0x27, 0xd7, 0xbb, 0x9f, 0x96, 0xba, 0xfa, 0x8a,
                    0x9c, 0x1e, 0x80, 0x6e, 0xfa, 0xa5, 0xd6, 0xba, 0xd7, 0x92, 0x71, 0xe9, 0x4e,
                    0x82, 0xa9, 0x02, 0x2a, 0x3b, 0xb8, 0x4e, 0x01, 0x53, 0x34, 0xa6, 0x70, 0x61,
                    0x56, 0x95, 0x1b, 0x59, 0xfe, 0x46, 0x94, 0x84, 0x8c, 0xa2, 0x2a, 0x16, 0x0c,
                    0xc2, 0x59, 0x9e, 0xac, 0xca, 0xa9, 0x93, 0xe6, 0x84, 0xf4,
                ],
                [
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
                    0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
                    0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26,
                    0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33,
                    0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
                ],
            )
        }

        #[test]
        fn test_check_cert_bytes_legacy() {
            let (legacy_cert_bytes, val) = cert_and_hw_id_legacy();

            let dummy_x509: X509Certificate =
                X509Certificate::from_der(&legacy_cert_bytes).unwrap().1;
            let extensions = dummy_x509.extensions_map().unwrap();

            let ext = extensions.get(&SnpOid::HwId.oid()).unwrap();

            assert!(check_cert_bytes(ext, &val));
        }

        #[test]
        fn test_check_cert_bytes() {
            let (cert_bytes, val) = cert_and_hw_id();

            let dummy_x509: X509Certificate = X509Certificate::from_der(&cert_bytes).unwrap().1;
            let extensions = dummy_x509.extensions_map().unwrap();

            let ext = extensions.get(&SnpOid::HwId.oid()).unwrap();

            assert!(check_cert_bytes(ext, val.as_slice()));
        }

        #[test]
        fn test_check_cert_bytes_integer() {
            let (cert_bytes, _) = cert_and_hw_id();
            let val = 0x1Eu8;
            let dummy_x509: X509Certificate = X509Certificate::from_der(&cert_bytes).unwrap().1;
            let extensions = dummy_x509.extensions_map().unwrap();
            let ext = extensions.get(&SnpOid::Ucode.oid()).unwrap();
            assert!(check_cert_bytes(ext, &val.to_ne_bytes()));
        }
    }
}
