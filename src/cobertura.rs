use quick_xml::{
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
    Writer,
};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::BTreeSet,
    io::{BufWriter, Cursor, Write},
    iter,
};
use symbolic_common::Name;
use symbolic_demangle::{Demangle, DemangleOptions};

use crate::defs::CovResultIter;
use crate::output::get_target_output_writable;

macro_rules! demangle {
    ($name: expr, $demangle: expr, $options: expr) => {{
        if $demangle {
            Name::from($name)
                .demangle($options)
                .unwrap_or_else(|| $name.clone())
        } else {
            $name.clone()
        }
    }};
}

// http://cobertura.sourceforge.net/xml/coverage-04.dtd

struct Coverage {
    sources: Vec<String>,
    packages: Vec<Package>,
}

struct CoverageStats {
    lines_covered: f64,
    lines_valid: f64,
    branches_covered: f64,
    branches_valid: f64,
    complexity: f64,
}

impl CoverageStats {
    fn from_lines(lines: Lines, same_lines: Lines) -> Self {
        let (lines_valid, lines_covered) = lines.fold((0.0, 0.0), |(v, c), (_, l)| {
            if l.covered() {
                (v + 1.0, c + 1.0)
            } else {
                (v + 1.0, c)
            }
        });

        let branches: Vec<Vec<Condition>> = same_lines
            .into_iter()
            .filter_map(|(_, l)| match l {
                Line::Branch { conditions, .. } => Some(conditions),
                Line::Plain { .. } => None,
            })
            .collect();
        let (branches_covered, branches_valid) =
            branches
                .iter()
                .fold((0.0, 0.0), |(covered, valid), conditions| {
                    (
                        covered + conditions.iter().fold(0.0, |hits, c| c.coverage + hits),
                        valid + conditions.len() as f64,
                    )
                });

        Self {
            lines_valid,
            lines_covered,
            branches_valid,
            branches_covered,
            // for now always 0
            complexity: 0.0,
        }
    }

    fn line_rate(&self) -> f64 {
        if self.lines_valid > 0.0 {
            self.lines_covered / self.lines_valid
        } else {
            0.0
        }
    }
    fn branch_rate(&self) -> f64 {
        if self.branches_valid > 0.0 {
            self.branches_covered / self.branches_valid
        } else {
            0.0
        }
    }
}

type Lines<'a> = Box<dyn Iterator<Item = (u32, Line)> + 'a>;

trait Stats {
    fn get_lines<'a>(&'a self) -> Lines<'a>;

    fn get_stats(&self) -> CoverageStats {
        CoverageStats::from_lines(self.get_lines(), self.get_lines())
    }
}

impl Stats for Coverage {
    fn get_lines<'a>(&'a self) -> Lines<'a> {
        self.packages.get_lines()
    }
}

struct Package {
    name: String,
    classes: Vec<Class>,
}

impl Stats for Package {
    fn get_lines(&self) -> Lines {
        self.classes.get_lines()
    }
}

struct Class {
    name: String,
    file_name: String,
    lines: Vec<Line>,
    methods: Vec<Method>,
}

impl Stats for Class {
    fn get_lines(&self) -> Lines {
        self.methods.get_lines()
    }
}

struct Method {
    name: String,
    signature: String,
    lines: Vec<Line>,
}

impl Stats for Method {
    fn get_lines(&self) -> Lines {
        self.lines.get_lines()
    }
}

impl<T: Stats> Stats for Vec<T> {
    fn get_lines(&self) -> Lines {
        Box::new(self.into_iter().flat_map(|i| i.get_lines()))
    }
}

#[derive(Debug, Clone)]
enum Line {
    Plain {
        number: u32,
        hits: u64,
    },

    Branch {
        number: u32,
        hits: u64,
        conditions: Vec<Condition>,
    },
}

impl Line {
    fn number(&self) -> u32 {
        match self {
            Line::Plain { number, .. } | Line::Branch { number, .. } => *number,
        }
    }

    fn covered(&self) -> bool {
        match self {
            Line::Plain { hits, .. } | Line::Branch { hits, .. } if *hits > 0 => true,
            _ => false,
        }
    }
}

impl Stats for Line {
    fn get_lines(&self) -> Lines {
        Box::new(iter::once((self.number(), self.clone())))
    }
}

#[derive(Debug, Clone)]
struct Condition {
    number: usize,
    cond_type: ConditionType,
    coverage: f64,
}

// Condition types
#[derive(Debug, Clone)]
enum ConditionType {
    Jump,
}

impl ToString for ConditionType {
    fn to_string(&self) -> String {
        match *self {
            Self::Jump => String::from("jump"),
        }
    }
}

fn get_coverage(
    results: CovResultIter,
    demangle: bool,
    demangle_options: DemangleOptions,
) -> Coverage {
    let sources = vec![".".to_owned()];
    let packages: Vec<Package> = results
        .map(|(_, rel_path, result)| {
            let all_lines: Vec<u32> = result.lines.iter().map(|(k, _)| k).cloned().collect();

            let mut orphan_lines: BTreeSet<u32> = all_lines.iter().cloned().collect();

            let end: u32 = result.lines.keys().last().unwrap_or(&0) + 1;

            let mut start_indexes: Vec<u32> = Vec::new();
            for function in result.functions.values() {
                start_indexes.push(function.start);
            }
            start_indexes.sort_unstable();

            let functions = result.functions;
            let result_lines = result.lines;
            let result_branches = result.branches;

            let line_from_number = |number| {
                let hits = result_lines.get(&number).cloned().unwrap_or_default();
                if let Some(branches) = result_branches.get(&number) {
                    let conditions = branches
                        .iter()
                        .enumerate()
                        .map(|(i, b)| Condition {
                            cond_type: ConditionType::Jump,
                            coverage: if *b { 1.0 } else { 0.0 },
                            number: i,
                        })
                        .collect::<Vec<_>>();
                    Line::Branch {
                        number,
                        hits,
                        conditions,
                    }
                } else {
                    Line::Plain { number, hits }
                }
            };

            let methods: Vec<Method> = functions
                .iter()
                .map(|(name, function)| {
                    let mut func_end = end;

                    for start in &start_indexes {
                        if *start > function.start {
                            func_end = *start;
                            break;
                        }
                    }

                    let mut lines_in_function: Vec<u32> = Vec::new();
                    for line in all_lines
                        .iter()
                        .filter(|&&x| x >= function.start && x < func_end)
                    {
                        lines_in_function.push(*line);
                        orphan_lines.remove(line);
                    }

                    let lines: Vec<Line> = lines_in_function
                        .into_iter()
                        .map(line_from_number)
                        .collect();

                    Method {
                        name: demangle!(name, demangle, demangle_options),
                        signature: String::new(),
                        lines,
                    }
                })
                .collect();

            let lines: Vec<Line> = orphan_lines.into_iter().map(line_from_number).collect();
            let class = Class {
                name: rel_path
                    .file_stem()
                    .map(|x| x.to_str().unwrap())
                    .unwrap_or_default()
                    .to_string(),
                file_name: rel_path.to_str().unwrap_or_default().to_string(),
                lines,
                methods,
            };

            Package {
                name: rel_path.to_str().unwrap_or_default().to_string(),
                classes: vec![class],
            }
        })
        .collect();

    Coverage { sources, packages }
}

pub fn output_cobertura(results: CovResultIter, output_file: Option<&str>, demangle: bool) {
    let demangle_options = DemangleOptions::name_only();

    let coverage = get_coverage(results, demangle, demangle_options);

    let mut writer = Writer::new_with_indent(Cursor::new(vec![]), b' ', 4);
    writer
        .write_event(Event::Decl(BytesDecl::new(b"1.0", None, None)))
        .unwrap();
    writer
        .write_event(Event::DocType(BytesText::from_escaped_str(
            " coverage SYSTEM 'http://cobertura.sourceforge.net/xml/coverage-04.dtd'",
        )))
        .unwrap();

    let cov_tag = b"coverage";
    let mut cov = BytesStart::borrowed(cov_tag, cov_tag.len());
    let stats = coverage.get_stats();
    cov.push_attribute(("lines-covered", stats.lines_covered.to_string().as_ref()));
    cov.push_attribute(("lines-valid", stats.lines_valid.to_string().as_ref()));
    cov.push_attribute(("line-rate", stats.line_rate().to_string().as_ref()));
    cov.push_attribute((
        "branches-covered",
        stats.branches_covered.to_string().as_ref(),
    ));
    cov.push_attribute(("branches-valid", stats.branches_valid.to_string().as_ref()));
    cov.push_attribute(("branch-rate", stats.branch_rate().to_string().as_ref()));
    cov.push_attribute(("complexity", "0"));
    cov.push_attribute(("version", "1.9"));

    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(s) => s.as_secs().to_string(),
        Err(_) => String::from("0"),
    };
    cov.push_attribute(("timestamp", secs.as_ref()));

    writer.write_event(Event::Start(cov)).unwrap();

    // export header
    let sources_tag = b"sources";
    let source_tag = b"source";
    writer
        .write_event(Event::Start(BytesStart::borrowed(
            sources_tag,
            sources_tag.len(),
        )))
        .unwrap();
    for path in &coverage.sources {
        writer
            .write_event(Event::Start(BytesStart::borrowed(
                source_tag,
                source_tag.len(),
            )))
            .unwrap();
        writer.write(path.as_bytes()).unwrap();
        writer
            .write_event(Event::End(BytesEnd::borrowed(source_tag)))
            .unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::borrowed(sources_tag)))
        .unwrap();

    // export packages
    let packages_tag = b"packages";
    let pack_tag = b"package";

    writer
        .write_event(Event::Start(BytesStart::borrowed(
            packages_tag,
            packages_tag.len(),
        )))
        .unwrap();
    // Export the package
    for package in &coverage.packages {
        let mut pack = BytesStart::borrowed(pack_tag, pack_tag.len());
        pack.push_attribute(("name", package.name.as_ref()));
        let stats = package.get_stats();
        pack.push_attribute(("line-rate", stats.line_rate().to_string().as_ref()));
        pack.push_attribute(("branch-rate", stats.branch_rate().to_string().as_ref()));
        pack.push_attribute(("complexity", stats.complexity.to_string().as_ref()));

        writer.write_event(Event::Start(pack)).unwrap();

        // export_classes
        let classes_tag = b"classes";
        let class_tag = b"class";
        let methods_tag = b"methods";
        let method_tag = b"method";

        writer
            .write_event(Event::Start(BytesStart::borrowed(
                classes_tag,
                classes_tag.len(),
            )))
            .unwrap();

        for class in &package.classes {
            let mut c = BytesStart::borrowed(class_tag, class_tag.len());
            c.push_attribute(("name", class.name.as_ref()));
            c.push_attribute(("filename", class.file_name.as_ref()));
            let stats = class.get_stats();
            c.push_attribute(("line-rate", stats.line_rate().to_string().as_ref()));
            c.push_attribute(("branch-rate", stats.branch_rate().to_string().as_ref()));
            c.push_attribute(("complexity", stats.complexity.to_string().as_ref()));

            writer.write_event(Event::Start(c)).unwrap();
            writer
                .write_event(Event::Start(BytesStart::borrowed(
                    methods_tag,
                    methods_tag.len(),
                )))
                .unwrap();

            for method in &class.methods {
                let mut m = BytesStart::borrowed(method_tag, method_tag.len());
                m.push_attribute(("name", method.name.as_ref()));
                m.push_attribute(("signature", method.signature.as_ref()));
                let stats = method.get_stats();
                m.push_attribute(("line-rate", stats.line_rate().to_string().as_ref()));
                m.push_attribute(("branch-rate", stats.branch_rate().to_string().as_ref()));
                m.push_attribute(("complexity", stats.complexity.to_string().as_ref()));
                writer.write_event(Event::Start(m)).unwrap();

                write_lines(&mut writer, &method.lines);
                writer
                    .write_event(Event::End(BytesEnd::borrowed(method_tag)))
                    .unwrap();
            }
            writer
                .write_event(Event::End(BytesEnd::borrowed(methods_tag)))
                .unwrap();
            write_lines(&mut writer, &class.lines);
        }
        writer
            .write_event(Event::End(BytesEnd::borrowed(class_tag)))
            .unwrap();
        writer
            .write_event(Event::End(BytesEnd::borrowed(classes_tag)))
            .unwrap();
        writer
            .write_event(Event::End(BytesEnd::borrowed(pack_tag)))
            .unwrap();
    }

    writer
        .write_event(Event::End(BytesEnd::borrowed(packages_tag)))
        .unwrap();

    writer
        .write_event(Event::End(BytesEnd::borrowed(cov_tag)))
        .unwrap();

    let result = writer.into_inner().into_inner();
    let mut file = BufWriter::new(get_target_output_writable(output_file));
    file.write_all(&result).unwrap();
}

fn write_lines(writer: &mut Writer<Cursor<Vec<u8>>>, lines: &[Line]) {
    let lines_tag = b"lines";
    let line_tag = b"line";

    writer
        .write_event(Event::Start(BytesStart::borrowed(
            lines_tag,
            lines_tag.len(),
        )))
        .unwrap();
    for line in lines {
        let mut l = BytesStart::borrowed(line_tag, line_tag.len());
        match line {
            Line::Plain {
                ref number,
                ref hits,
            } => {
                l.push_attribute(("number", number.to_string().as_ref()));
                l.push_attribute(("hits", hits.to_string().as_ref()));
                writer.write_event(Event::Start(l)).unwrap();
            }
            Line::Branch {
                ref number,
                ref hits,
                conditions,
            } => {
                l.push_attribute(("number", number.to_string().as_ref()));
                l.push_attribute(("hits", hits.to_string().as_ref()));
                l.push_attribute(("branch", "true"));
                writer.write_event(Event::Start(l)).unwrap();

                let conditions_tag = b"conditions";
                let condition_tag = b"condition";

                writer
                    .write_event(Event::Start(BytesStart::borrowed(
                        conditions_tag,
                        conditions_tag.len(),
                    )))
                    .unwrap();
                for condition in conditions {
                    let mut c = BytesStart::borrowed(condition_tag, condition_tag.len());
                    c.push_attribute(("number", condition.number.to_string().as_ref()));
                    c.push_attribute(("type", condition.cond_type.to_string().as_ref()));
                    c.push_attribute(("coverage", condition.coverage.to_string().as_ref()));
                    writer.write_event(Event::Empty(c)).unwrap();
                }
                writer
                    .write_event(Event::End(BytesEnd::borrowed(conditions_tag)))
                    .unwrap();
            }
        }
        writer
            .write_event(Event::End(BytesEnd::borrowed(line_tag)))
            .unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::borrowed(lines_tag)))
        .unwrap();
}

#[cfg(test)]
mod tests {
    extern crate tempfile;
    use super::*;
    use crate::{CovResult, Function};
    use rustc_hash::FxHashMap;
    use std::fs::File;
    use std::io::Read;
    use std::{collections::BTreeMap, path::PathBuf};

    fn read_file(path: &PathBuf) -> String {
        let mut f =
            File::open(path).expect(format!("{:?} file not found", path.file_name()).as_str());
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn test_cobertura() {
        /* main.rs
        fn main() {
            let inp = "a";
            if "a" == inp {
                println!("a");
            } else if "b" == inp {
                println!("b");
            }
            println!("what?");
        }
        */

        let tmp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let file_name = "test_cobertura.xml";
        let file_path = tmp_dir.path().join(&file_name);

        let results = vec![(
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/main.rs"),
            CovResult {
                lines: [
                    (1, 1),
                    (2, 1),
                    (3, 2),
                    (4, 1),
                    (5, 0),
                    (6, 0),
                    (8, 1),
                    (9, 1),
                ]
                .iter()
                .cloned()
                .collect(),
                branches: {
                    let mut map = BTreeMap::new();
                    map.insert(3, vec![true, false]);
                    map.insert(5, vec![false, false]);
                    map
                },
                functions: {
                    let mut map = FxHashMap::default();
                    map.insert(
                        "_ZN8cov_test4main17h7eb435a3fb3e6f20E".to_string(),
                        Function {
                            start: 1,
                            executed: true,
                        },
                    );
                    map
                },
            },
        )];

        let results = Box::new(results.into_iter());
        output_cobertura(results, Some(file_path.to_str().unwrap()), true);

        let results = read_file(&file_path);

        assert!(results.contains(r#"package name="src/main.rs""#));
        assert!(results.contains(r#"class name="main" filename="src/main.rs""#));
        assert!(results.contains(r#"method name="cov_test::main""#));
        assert!(results.contains(r#"line number="1" hits="1">"#));
        assert!(results.contains(r#"line number="3" hits="2" branch="true""#));
        assert!(results.contains(r#"<condition number="0" type="jump" coverage="1"/>"#));

        assert!(results.contains(r#"lines-covered="6""#));
        assert!(results.contains(r#"lines-valid="8""#));
        assert!(results.contains(r#"line-rate="0.75""#));

        assert!(results.contains(r#"branches-covered="1""#));
        assert!(results.contains(r#"branches-valid="4""#));
        assert!(results.contains(r#"branch-rate="0.25""#));
    }

    #[test]
    fn test_cobertura_double_lines() {
        /* main.rs
        fn main() {
        }

        #[test]
        fn test_fn() {
            let s = "s";
            if s == "s" {
                println!("test");
            }
            println!("test");
        }
        */

        let tmp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let file_name = "test_cobertura.xml";
        let file_path = tmp_dir.path().join(&file_name);

        let results = vec![(
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/main.rs"),
            CovResult {
                lines: [
                    (1, 2),
                    (3, 0),
                    (6, 2),
                    (7, 1),
                    (8, 2),
                    (9, 1),
                    (11, 1),
                    (12, 2),
                ]
                .iter()
                .cloned()
                .collect(),
                branches: {
                    let mut map = BTreeMap::new();
                    map.insert(8, vec![true, false]);
                    map
                },
                functions: {
                    let mut map = FxHashMap::default();
                    map.insert(
                        "_ZN8cov_test7test_fn17hbf19ec7bfabe8524E".to_string(),
                        Function {
                            start: 6,
                            executed: true,
                        },
                    );

                    map.insert(
                        "_ZN8cov_test4main17h7eb435a3fb3e6f20E".to_string(),
                        Function {
                            start: 1,
                            executed: false,
                        },
                    );

                    map.insert(
                        "_ZN8cov_test4main17h29b45b3d7d8851d2E".to_string(),
                        Function {
                            start: 1,
                            executed: true,
                        },
                    );

                    map.insert(
                        "_ZN8cov_test7test_fn28_$u7b$$u7b$closure$u7d$$u7d$17hab7a162ac9b573fcE"
                            .to_string(),
                        Function {
                            start: 6,
                            executed: true,
                        },
                    );

                    map.insert(
                        "_ZN8cov_test4main17h679717cd8503f8adE".to_string(),
                        Function {
                            start: 1,
                            executed: false,
                        },
                    );
                    map
                },
            },
        )];

        let results = Box::new(results.into_iter());
        output_cobertura(results, Some(file_path.to_str().unwrap()), true);

        let results = read_file(&file_path);

        println!("{}", results);

        assert!(results.contains(r#"package name="src/main.rs""#));
        assert!(results.contains(r#"class name="main" filename="src/main.rs""#));
        assert!(results.contains(r#"method name="cov_test::main""#));
        assert!(results.contains(r#"method name="cov_test::test_fn""#));

        assert!(results.contains(r#"lines-covered="7""#));
        assert!(results.contains(r#"lines-valid="8""#));
        assert!(results.contains(r#"line-rate="0.875""#));

        assert!(results.contains(r#"branches-covered="1""#));
        assert!(results.contains(r#"branches-valid="2""#));
        assert!(results.contains(r#"branch-rate="0.5""#));
    }
}
