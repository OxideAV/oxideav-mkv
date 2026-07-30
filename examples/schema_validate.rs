//! Dev helper: validate any Matroska / WebM file against the staged
//! CELLAR schema (`cargo run --example schema_validate <file.mkv>`).
//!
//! Prints the headline verdict, the per-class counters, and every
//! finding with its absolute offset, element name (when the ID is in
//! the schema), and kind. Exit status: 0 when the document is valid
//! (informational findings allowed), 1 on violations or structural
//! damage, 2 on usage / I/O errors.

use oxideav_mkv::schema::{element_def, validate};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: schema_validate <file.mkv>");
        std::process::exit(2);
    };
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(2);
        }
    };
    let report = match validate(&mut f) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{path}: validation walk failed: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "{path}: DocType {:?} v{:?} — {} elements, {} violation(s), {} informational",
        report.doc_type.as_deref().unwrap_or("?"),
        report.doc_type_version.unwrap_or(0),
        report.elements_scanned,
        report.violations,
        report.informational,
    );
    for finding in &report.findings {
        let name = element_def(finding.id).map(|d| d.name).unwrap_or("?");
        let class = if finding.kind.is_violation() {
            "VIOLATION"
        } else {
            "info"
        };
        println!(
            "  {class:9} @0x{:08X} 0x{:X} {} — {:?}",
            finding.offset, finding.id, name, finding.kind
        );
    }
    if report.findings_truncated {
        println!("  … findings list truncated (counters above are exact)");
    }
    if let Some(off) = report.scan_stopped_at {
        println!("  STRUCTURAL DAMAGE: walk stopped at 0x{off:08X}");
    }
    if report.is_valid() {
        println!(
            "  VALID{}",
            if report.informational > 0 {
                " (with informational findings)"
            } else {
                ""
            }
        );
    } else {
        println!("  NOT VALID");
        std::process::exit(1);
    }
}
