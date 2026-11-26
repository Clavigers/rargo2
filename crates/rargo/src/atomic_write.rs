use std::path::{Path, PathBuf};

pub(crate) struct Plan<'a> {
    temporary: PathBuf,
    destination: &'a Path,
}

pub(crate) enum Effect<'a> {
    Write { path: &'a Path, bytes: &'a [u8] },
    Rename { from: &'a Path, to: &'a Path },
}

impl<'a> Plan<'a> {
    pub(crate) fn new(destination: &'a Path, pid: u32, sequence: u64) -> Self {
        let name = destination.file_name().unwrap_or_default().to_string_lossy();
        let temporary = destination.with_file_name(format!("{}{pid}-{sequence}-{name}", crate::tmp::BESIDE));
        Self { temporary, destination }
    }

    pub(crate) fn temporary(&self) -> &Path {
        &self.temporary
    }

    pub(crate) fn effects<'b>(&'b self, bytes: &'b [u8]) -> [Effect<'b>; 2] {
        [
            Effect::Write { path: self.temporary(), bytes },
            Effect::Rename { from: self.temporary(), to: self.destination },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn overlapping_writes_publish_complete_values_and_leave_only_the_destination() {
        let destination = Path::new("cache/shared");
        for (ids, sequences) in [([42, 42], [1, 2]), ([42, 43], [1, 1])] {
            let plans = [
                Plan::new(destination, ids[0], sequences[0]),
                Plan::new(destination, ids[1], sequences[1]),
            ];
            assert_ne!(plans[0].temporary(), plans[1].temporary());
            for plan in &plans {
                assert_eq!(plan.temporary().parent(), destination.parent());
            }

            let values: [&[u8]; 2] = [b"first value", b"second"];
            for order in [[0, 1], [1, 0]] {
                let mut files = BTreeMap::from([(destination, &b"previous"[..])]);
                let apply = |files: &mut BTreeMap<_, _>, effect| match effect {
                    Effect::Write { path, bytes } => {
                        assert_eq!(files.insert(path, bytes), None, "each writer needs its own staged file");
                    }
                    Effect::Rename { from, to } => {
                        let bytes = files.remove(from).expect("each writer still owns its staged file");
                        files.insert(to, bytes);
                    }
                };
                let [stage_first, publish_first] = plans[order[0]].effects(values[order[0]]);
                let [stage_second, publish_second] = plans[order[1]].effects(values[order[1]]);
                apply(&mut files, stage_second);
                apply(&mut files, stage_first);
                assert_eq!(files[destination], b"previous");
                apply(&mut files, publish_first);
                assert_eq!(files[destination], values[order[0]]);
                apply(&mut files, publish_second);
                assert_eq!(files, BTreeMap::from([(destination, values[order[1]])]));
            }
        }
    }
}
