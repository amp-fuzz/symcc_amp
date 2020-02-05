use anyhow::{ensure, Context, Result};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use structopt::StructOpt;
use tempfile::tempdir;

const TIMEOUT: u32 = 90;

#[derive(Debug, StructOpt)]
#[structopt(about = "Make SymCC collaborate with AFL.", no_version)]
struct CLI {
    /// The name of the fuzzer to work with
    #[structopt(short = "a")]
    fuzzer_name: String,

    /// The AFL output directory
    #[structopt(short = "o")]
    output_dir: PathBuf,

    /// Name to use for SymCC
    #[structopt(short = "n")]
    name: String,

    /// Program under test
    command: Vec<String>,
}

fn main() -> Result<()> {
    let options = CLI::from_args();
    if !options.output_dir.is_dir() {
        println!(
            "The directory {} does not exist!",
            options.output_dir.display()
        );
        return Ok(());
    }

    let afl_queue = options.output_dir.join(&options.fuzzer_name).join("queue");
    if !afl_queue.is_dir() {
        println!("The AFL queue {} does not exist!", afl_queue.display());
        return Ok(());
    }

    let symcc_dir = options.output_dir.join(&options.name);
    fs::create_dir(&symcc_dir)
        .with_context(|| format!("Failed to create SymCC's directory {}", symcc_dir.display()))?;
    let symcc_queue = symcc_dir.join("queue");
    fs::create_dir(&symcc_queue)
        .with_context(|| format!("Failed to create SymCC's queue {}", symcc_queue.display()))?;
    let current_input = symcc_dir.join(".cur_input");

    let use_standard_input = !options.command.contains(&String::from("@@"));
    let fixed_command = insert_input_file(&options.command, &current_input);

    let mut analysis_command = Command::new("timeout");
    analysis_command
        .args(&["-k", "5", &TIMEOUT.to_string()])
        .args(&fixed_command)
        .env("SYMCC_ENABLE_LINEARIZATION", "1")
        .env("SYMCC_AFL_COVERAGE_MAP", symcc_dir.join("bitmap"));
    if use_standard_input {
        analysis_command.stdin(Stdio::piped());
    } else {
        analysis_command.stdin(Stdio::null());
        analysis_command.env("SYMCC_INPUT_FILE", &current_input);
    }

    let afl_stats_file_path = options
        .output_dir
        .join(&options.fuzzer_name)
        .join("fuzzer_stats");
    let mut afl_stats_file = File::open(&afl_stats_file_path).with_context(|| {
        format!(
            "Failed to open the fuzzer's stats at {}",
            afl_stats_file_path.display()
        )
    })?;
    let mut afl_stats = String::new();
    afl_stats_file
        .read_to_string(&mut afl_stats)
        .with_context(|| {
            format!(
                "Failed to read the fuzzer's stats at {}",
                afl_stats_file_path.display()
            )
        })?;
    let afl_command: Vec<_> = afl_stats
        .lines()
        .find(|&l| l.starts_with("command_line"))
        .expect("The fuzzer stats don't contain the command line")
        .splitn(2, ':')
        .skip(1)
        .next()
        .expect("The fuzzer stats follow an unknown format")
        .trim()
        .split_whitespace()
        .collect();
    let afl_target_command: Vec<_> = afl_command
        .iter()
        .skip_while(|s| **s != "--")
        .map(|s| OsString::from(s))
        .collect();
    let afl_binary_dir = Path::new(
        afl_command
            .get(0)
            .expect("The AFL command is unexpectedly short"),
    )
    .parent()
    .unwrap();
    let afl_show_map = afl_binary_dir.join("afl-showmap");

    let mut processed_files = HashSet::new();
    let mut current_test_id = 0u64;
    let mut bitmap = [0u8; 65536];
    loop {
        let mut test_files: Vec<_> = fs::read_dir(&afl_queue)
            .with_context(|| {
                format!(
                    "Failed to open the fuzzer's queue at {}",
                    afl_queue.display()
                )
            })?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()
            .with_context(|| {
                format!(
                    "Failed to read the fuzzer's queue at {}",
                    afl_queue.display()
                )
            })?
            .into_iter()
            .filter(|path| path.is_file() && !processed_files.contains(path))
            .collect();

        test_files.sort_by_cached_key(|path| dbg!(TestcaseScore::new(path)));
        test_files.reverse();

        if test_files.is_empty() {
            println!("Waiting for new test cases...");
            thread::sleep(Duration::from_secs(5));
        } else {
            for input in test_files {
                println!("Running on input {}", input.display());
                fs::copy(&input, &current_input).with_context(|| {
                    format!(
                        "Failed to copy the test case {} to our workbench at {}",
                        input.display(),
                        current_input.display()
                    )
                })?;

                let tmp_dir = tempdir().context(
                    "Failed to create a temporary directory for this execution of SymCC",
                )?;
                let output_dir = tmp_dir.path().join("output");
                fs::create_dir(&output_dir).with_context(|| {
                    format!(
                        "Failed to create the output directory {} for SymCC",
                        output_dir.display()
                    )
                })?;
                let testcase_bitmap_file = tmp_dir.path().join("testcase_bitmap");

                let log_file_path = tmp_dir.path().join("symcc.log");
                let log_file = File::create(&log_file_path).with_context(|| {
                    format!(
                        "Failed to create SymCC's log file at {}",
                        log_file_path.display()
                    )
                })?;
                let mut child = analysis_command
                    .env("SYMCC_OUTPUT_DIR", &output_dir)
                    .stdout(
                        log_file
                            .try_clone()
                            .context("Failed to open SymCC's log file a second time")?,
                    )
                    .stderr(log_file)
                    .spawn()
                    .context("Failed to run SymCC")?;

                if use_standard_input {
                    io::copy(
                        &mut File::open(&current_input).with_context(|| {
                            format!(
                                "Failed to read the test input at {}",
                                current_input.display()
                            )
                        })?,
                        child
                            .stdin
                            .as_mut()
                            .expect("Failed to pipe to the child's standard input"),
                    )
                    .context("Failed to pipe the test input to SymCC")?;
                }

                let result = child.wait().context("Failed to wait for SymCC")?;
                if let Some(code) = result.code() {
                    println!("SymCC returned code {}", code);
                }

                let orig_id = input
                    .file_name()
                    .expect("The input file does not have a name")
                    .to_string_lossy();

                for maybe_new_test in fs::read_dir(&output_dir).with_context(|| {
                    format!(
                        "Failed to read the generated test cases at {}",
                        output_dir.display()
                    )
                })? {
                    let new_test = maybe_new_test.with_context(|| {
                        format!(
                            "Failed to read all test cases from {}",
                            output_dir.display()
                        )
                    })?;
                    let mut afl_show_map_child = Command::new(&afl_show_map)
                        .args(&["-t", "5000", "-m", "none", "-b", "-o"])
                        .arg(&testcase_bitmap_file)
                        .args(insert_input_file(&afl_target_command, new_test.path()))
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .stdin(if use_standard_input {
                            Stdio::piped()
                        } else {
                            Stdio::null()
                        })
                        .spawn()
                        .context("Failed to run afl-showmap")?;

                    if use_standard_input {
                        io::copy(
                            &mut File::open(new_test.path())?,
                            afl_show_map_child
                                .stdin
                                .as_mut()
                                .expect("Failed to open the stardard input of afl-showmap"),
                        )
                        .context("Failed to pipe the test input to afl-showmap")?;
                    }

                    let afl_show_map_status = afl_show_map_child
                        .wait()
                        .context("Failed to wait for afl-showmap")?;
                    ensure!(
                        afl_show_map_status.success(),
                        "afl-showmap failed with code {}",
                        if let Some(code) = afl_show_map_status.code() {
                            code.to_string()
                        } else {
                            "(no code)".to_owned()
                        }
                    );

                    let testcase_bitmap = fs::read(&testcase_bitmap_file).with_context(|| {
                        format!(
                            "Failed to read the AFL bitmap that \
                             afl-showmap should have generated at {}",
                            testcase_bitmap_file.display()
                        )
                    })?;
                    ensure!(
                        testcase_bitmap.len() == bitmap.len(),
                        "The map generated by afl-showmap has the wrong size ({})",
                        testcase_bitmap.len()
                    );

                    let mut interesting = false;
                    for (known, new) in bitmap.iter_mut().zip(testcase_bitmap) {
                        if *known != (*known | new) {
                            *known |= new;
                            interesting = true;
                        }
                    }

                    if interesting {
                        let new_name = format!("id:{:06},src:{}", current_test_id, &orig_id);
                        fs::copy(new_test.path(), symcc_queue.join(new_name)).with_context(
                            || {
                                format!(
                                    "Failed to copy the test case {} to our queue at {}",
                                    new_test.path().display(),
                                    symcc_queue.display()
                                )
                            },
                        )?;
                        current_test_id += 1;
                    }
                }

                processed_files.insert(input);
            }
        }
    }
}

fn insert_input_file<S: AsRef<OsStr>, P: AsRef<Path>>(
    command: &[S],
    input_file: P,
) -> Vec<OsString> {
    let mut fixed_command: Vec<OsString> = command.iter().map(|s| s.into()).collect();
    if let Some(at_signs) = fixed_command.iter_mut().find(|s| *s == "@@") {
        *at_signs = input_file.as_ref().as_os_str().to_os_string();
    }

    fixed_command
}

/// Score of a test case.
///
/// We use the lexical comparison implemented by the derived implementation of
/// Ord in order to compare according to various criteria.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct TestcaseScore {
    /// First criterion: new coverage
    new_coverage: bool,

    /// Second criterion: being derived from seed inputs
    derived_from_seed: bool,

    /// Third criterion: size (smaller is better)
    file_size: i128,

    /// Fourth criterion: name (containing the ID)
    base_name: OsString,
}

impl TestcaseScore {
    /// Score a test case.
    ///
    /// If anything goes wrong, return the minimum score.
    fn new<P: AsRef<Path>>(t: P) -> TestcaseScore {
        let size = match fs::metadata(&t) {
            Err(e) => {
                // Has the file disappeared?
                eprintln!(
                    "Warning: failed to score test case {}: {}",
                    t.as_ref().display(),
                    e
                );

                return TestcaseScore::minimum();
            }
            Ok(meta) => meta.len(),
        };

        let name: OsString = match t.as_ref().file_name() {
            None => return TestcaseScore::minimum(),
            Some(n) => n.to_os_string(),
        };
        let name_string = name.to_string_lossy();

        TestcaseScore {
            new_coverage: name_string.ends_with("+cov"),
            derived_from_seed: name_string.contains("orig:"),
            file_size: i128::from(size) * -1,
            base_name: name,
        }
    }

    /// Return the smallest possible score.
    fn minimum() -> TestcaseScore {
        TestcaseScore {
            new_coverage: false,
            derived_from_seed: false,
            file_size: std::i128::MIN,
            base_name: OsString::from(""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_ordering() {
        let min_score = TestcaseScore::minimum();
        assert!(
            TestcaseScore {
                new_coverage: true,
                ..TestcaseScore::minimum()
            } > min_score
        );
        assert!(
            TestcaseScore {
                derived_from_seed: true,
                ..TestcaseScore::minimum()
            } > min_score
        );
        assert!(
            TestcaseScore {
                file_size: -4,
                ..TestcaseScore::minimum()
            } > min_score
        );
        assert!(
            TestcaseScore {
                base_name: OsString::from("foo"),
                ..TestcaseScore::minimum()
            } > min_score
        );
    }
}
