use rargo_proto::{FutureReport, REPORT_ID};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const VERSION: u32 = 0;
const MAX_REPORTS: usize = 5;

#[derive(Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    next_id: u32,
    reports: Vec<Report>,
}

#[derive(Serialize, Deserialize)]
struct Report {
    id: u32,
    suggestion_message: String,
    per_package: BTreeMap<String, String>,
}

pub fn save(r: &FutureReport) -> std::io::Result<u32> {
    let path = Path::new(&r.path);
    let mut disk = rargo_net::io::file::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<OnDisk>(&b).ok())
        .filter(|d| d.version == VERSION)
        .unwrap_or(OnDisk { version: VERSION, next_id: 1, reports: Vec::new() });
    let per_package: BTreeMap<String, String> = r.per_package.iter().cloned().collect();
    if let Some(have) = disk.reports.iter().find(|x| x.per_package == per_package) {
        return Ok(have.id);
    }
    let id = disk.next_id;
    disk.next_id += 1;
    disk.reports.push(Report { id, suggestion_message: r.suggestion.replace(REPORT_ID, &id.to_string()), per_package });
    if disk.reports.len() > MAX_REPORTS {
        disk.reports.remove(0);
    }
    crate::write_atomic(path, &serde_json::to_vec(&disk).map_err(std::io::Error::other)?)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(path: &Path, text: &str) -> FutureReport {
        FutureReport {
            path: path.to_string_lossy().into_owned(),
            per_package: vec![("old@0.1.0".into(), text.into())],
            suggestion: format!("run `cargo report future-incompatibilities --id {REPORT_ID} --package old@0.1.0`"),
        }
    }

    #[test]
    fn reports_are_numbered_kept_and_not_repeated() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("target/.future-incompat-report.json");
        assert_eq!(save(&report(&p, "a")).unwrap(), 1);
        assert_eq!(save(&report(&p, "b")).unwrap(), 2);
        assert_eq!(save(&report(&p, "a")).unwrap(), 1);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 0);
        assert_eq!(v["next_id"], 3);
        assert_eq!(v["reports"][1]["id"], 2);
        assert_eq!(v["reports"][1]["per_package"]["old@0.1.0"], "b");
        assert_eq!(
            v["reports"][1]["suggestion_message"],
            "run `cargo report future-incompatibilities --id 2 --package old@0.1.0`"
        );
        for t in ["c", "d", "e", "f"] {
            save(&report(&p, t)).unwrap();
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let ids: Vec<u64> = v["reports"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![2, 3, 4, 5, 6]);
        std::fs::write(&p, br#"{"version":9,"next_id":40,"reports":[]}"#).unwrap();
        assert_eq!(save(&report(&p, "a")).unwrap(), 1);
    }
}
