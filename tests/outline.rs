use std::path::{Path, PathBuf};

use assert_cmd::Command;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use pdq::{read_outline, read_outline_with_password};
use predicates::prelude::*;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn pdq() -> Command {
    Command::cargo_bin("pdq").unwrap()
}

/// Write a PDF with `pages` blank pages. `build` adds outline objects and
/// returns the entries to merge into the catalog.
fn write_pdf(
    path: &Path,
    pages: usize,
    build: impl FnOnce(&mut Document, &[ObjectId]) -> Dictionary,
) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let page_ids: Vec<ObjectId> = (0..pages)
        .map(|_| {
            let content = document.add_object(Object::Stream(Stream::new(
                Dictionary::new(),
                b"q Q".to_vec(),
            )));
            document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Resources" => dictionary! {},
                "Contents" => content,
            })
        })
        .collect();
    document.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
            "Count" => pages as i64,
        }
        .into(),
    );
    let mut catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    };
    catalog.extend(&build(&mut document, &page_ids));
    document.objects.insert(catalog_id, catalog.into());
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save outline fixture: {err}"));
}

struct Item {
    title: Object,
    target: Dictionary,
    children: Vec<Item>,
}

fn item(title: &str, target: Dictionary, children: Vec<Item>) -> Item {
    Item {
        title: Object::string_literal(title),
        target,
        children,
    }
}

fn fit(page: ObjectId) -> Object {
    Object::Array(vec![page.into(), "Fit".into()])
}

fn dest(page: ObjectId) -> Dictionary {
    dictionary! { "Dest" => fit(page) }
}

/// Add an outline root with `items` linked as a well-formed tree.
fn add_outline(document: &mut Document, items: Vec<Item>) -> ObjectId {
    let root_id = document.new_object_id();
    let (first_last, count) = add_items(document, root_id, items);
    let mut root = dictionary! { "Type" => "Outlines", "Count" => count };
    if let Some((first, last)) = first_last {
        root.set("First", first);
        root.set("Last", last);
    }
    document.objects.insert(root_id, root.into());
    root_id
}

fn add_items(
    document: &mut Document,
    parent: ObjectId,
    items: Vec<Item>,
) -> (Option<(ObjectId, ObjectId)>, i64) {
    let ids: Vec<ObjectId> = items.iter().map(|_| document.new_object_id()).collect();
    let mut count = 0;
    for (index, item) in items.into_iter().enumerate() {
        let (children, child_count) = add_items(document, ids[index], item.children);
        let mut dict = dictionary! { "Title" => item.title, "Parent" => parent };
        dict.extend(&item.target);
        if index > 0 {
            dict.set("Prev", ids[index - 1]);
        }
        if index + 1 < ids.len() {
            dict.set("Next", ids[index + 1]);
        }
        if let Some((first, last)) = children {
            dict.set("First", first);
            dict.set("Last", last);
            dict.set("Count", child_count);
        }
        document.objects.insert(ids[index], dict.into());
        count += 1 + child_count;
    }
    let first_last = ids.first().copied().zip(ids.last().copied());
    (first_last, count)
}

fn pages_by_title(path: &Path) -> Vec<(String, Option<usize>)> {
    read_outline(path)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| (entry.title, entry.page))
        .collect()
}

#[test]
fn outline_cli_reports_titles_depths_and_one_based_pages() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("book.pdf");
    write_pdf(&path, 4, |document, pages| {
        let root = add_outline(
            document,
            vec![
                item("Cover", dest(pages[0]), vec![]),
                item(
                    "Chapter 1",
                    dest(pages[1]),
                    vec![item("1.1 Background", dest(pages[2]), vec![])],
                ),
                item("Chapter 2", dest(pages[3]), vec![]),
            ],
        );
        dictionary! { "Outlines" => root }
    });

    pdq()
        .arg("outline")
        .arg(&path)
        .assert()
        .success()
        .stdout(concat!(
            "{\"pages\":4,\"outline\":[",
            "{\"title\":\"Cover\",\"page\":1,\"depth\":0,\"degraded\":false},",
            "{\"title\":\"Chapter 1\",\"page\":2,\"depth\":0,\"degraded\":false},",
            "{\"title\":\"1.1 Background\",\"page\":3,\"depth\":1,\"degraded\":false},",
            "{\"title\":\"Chapter 2\",\"page\":4,\"depth\":0,\"degraded\":false}",
            "]}\n"
        ));
}

#[test]
fn named_destinations_and_goto_actions_resolve_and_other_actions_do_not() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("named.pdf");
    write_pdf(&path, 4, |document, pages| {
        // PDF 1.2 name tree with one level of /Kids; the second value wraps
        // its destination in a dictionary's /D.
        let leaf = document.add_object(dictionary! {
            "Names" => vec![
                Object::string_literal("chapter-2"),
                fit(pages[2]),
                Object::string_literal("wrapped"),
                dictionary! { "D" => fit(pages[3]) }.into(),
            ],
            "Limits" => vec![Object::string_literal("chapter-2"), Object::string_literal("wrapped")],
        });
        let tree = document.add_object(dictionary! { "Kids" => vec![Object::Reference(leaf)] });
        let goto_chapter_2 =
            dictionary! { "S" => "GoTo", "D" => Object::string_literal("chapter-2") };
        let uri =
            dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") };
        let remote = dictionary! {
            "S" => "GoToR",
            "F" => Object::string_literal("other.pdf"),
            "D" => vec![Object::Integer(0), "Fit".into()],
        };
        let root = add_outline(
            document,
            vec![
                // PDF 1.1: a name looked up in the catalog /Dests dictionary.
                item("Intro", dictionary! { "Dest" => "intro" }, vec![]),
                item(
                    "Chapter 2",
                    dictionary! { "Dest" => Object::string_literal("chapter-2") },
                    vec![],
                ),
                item(
                    "Wrapped",
                    dictionary! { "Dest" => Object::string_literal("wrapped") },
                    vec![],
                ),
                item("Via action", dictionary! { "A" => goto_chapter_2 }, vec![]),
                item("Web", dictionary! { "A" => uri }, vec![]),
                item("Remote", dictionary! { "A" => remote }, vec![]),
                item("Missing", dictionary! { "Dest" => "nowhere" }, vec![]),
            ],
        );
        dictionary! {
            "Outlines" => root,
            "Names" => dictionary! { "Dests" => tree },
            "Dests" => dictionary! {
                "intro" => vec![pages[1].into(), "XYZ".into(), Object::Null, Object::Null, Object::Null],
            },
        }
    });

    assert_eq!(
        pages_by_title(&path),
        vec![
            ("Intro".to_string(), Some(2)),
            ("Chapter 2".to_string(), Some(3)),
            ("Wrapped".to_string(), Some(4)),
            ("Via action".to_string(), Some(3)),
            ("Web".to_string(), None),
            ("Remote".to_string(), None),
            ("Missing".to_string(), None),
        ]
    );
}

#[test]
fn destinations_outside_the_document_are_null_not_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stale.pdf");
    write_pdf(&path, 3, |document, pages| {
        let not_a_page = document.add_object(dictionary! { "Type" => "Font" });
        let root = add_outline(
            document,
            vec![
                item(
                    "Not a page",
                    dictionary! { "Dest" => fit(not_a_page) },
                    vec![],
                ),
                item("Dangling", dictionary! { "Dest" => fit((999, 0)) }, vec![]),
                item(
                    "Past the end",
                    dictionary! { "Dest" => vec![Object::Integer(99), "Fit".into()] },
                    vec![],
                ),
                // A 0-based page index in a local destination, as some writers produce.
                item(
                    "Page index",
                    dictionary! { "Dest" => vec![Object::Integer(1), "Fit".into()] },
                    vec![],
                ),
                item("Last", dest(pages[2]), vec![]),
            ],
        );
        dictionary! { "Outlines" => root }
    });

    assert_eq!(
        pages_by_title(&path),
        vec![
            ("Not a page".to_string(), None),
            ("Dangling".to_string(), None),
            ("Past the end".to_string(), None),
            ("Page index".to_string(), Some(2)),
            ("Last".to_string(), Some(3)),
        ]
    );
}

#[test]
fn a_document_without_an_outline_reports_an_empty_list() {
    let dir = tempdir().unwrap();
    let plain = dir.path().join("plain.pdf");
    write_pdf(&plain, 2, |_, _| Dictionary::new());
    pdq()
        .arg("outline")
        .arg(&plain)
        .assert()
        .success()
        .stdout("{\"pages\":2,\"outline\":[]}\n");

    let empty_root = dir.path().join("empty-root.pdf");
    write_pdf(&empty_root, 1, |document, _| {
        let root = add_outline(document, Vec::new());
        dictionary! { "Outlines" => root }
    });
    assert!(read_outline(&empty_root).unwrap().entries.is_empty());
}

#[test]
fn looping_outline_chains_end_instead_of_hanging() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("loop.pdf");
    write_pdf(&path, 2, |document, pages| {
        let root = document.new_object_id();
        let a = document.new_object_id();
        let b = document.new_object_id();
        // A is its own child, B's /Next points back to A and its /First to the root.
        document.objects.insert(
            a,
            dictionary! {
                "Title" => Object::string_literal("A"),
                "Parent" => root,
                "Next" => b,
                "First" => a,
                "Dest" => fit(pages[0]),
            }
            .into(),
        );
        document.objects.insert(
            b,
            dictionary! {
                "Title" => Object::string_literal("B"),
                "Parent" => root,
                "Next" => a,
                "First" => root,
                "Dest" => fit(pages[1]),
            }
            .into(),
        );
        document.objects.insert(
            root,
            dictionary! { "Type" => "Outlines", "First" => a, "Last" => b }.into(),
        );
        dictionary! { "Outlines" => root }
    });

    let outline = read_outline(&path).unwrap();
    let entries: Vec<_> = outline
        .entries
        .iter()
        .map(|entry| (entry.title.as_str(), entry.depth, entry.page))
        .collect();
    assert_eq!(entries, vec![("A", 0, Some(1)), ("B", 0, Some(2))]);
}

#[test]
fn titles_decode_both_encodings_and_damage_is_flagged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("titles.pdf");
    write_pdf(&path, 1, |document, pages| {
        let titled = |title: Object| Item {
            title,
            target: dest(pages[0]),
            children: Vec::new(),
        };
        let root = add_outline(
            document,
            vec![
                titled(Object::String(
                    b"\xfe\xff\x00C\x00a\x00p\x00\xed\x00t\x00u\x00l\x00o".to_vec(),
                    StringFormat::Hexadecimal,
                )),
                titled(Object::String(
                    b"Se\xe7\xe3o".to_vec(),
                    StringFormat::Literal,
                )),
                titled(Object::String(b"bad\x01".to_vec(), StringFormat::Literal)),
                titled(Object::Integer(7)),
            ],
        );
        dictionary! { "Outlines" => root }
    });

    let outline = read_outline(&path).unwrap();
    let titles: Vec<_> = outline
        .entries
        .iter()
        .map(|entry| (entry.title.as_str(), entry.degraded))
        .collect();
    assert_eq!(
        titles,
        vec![
            ("Capítulo", false),
            ("Seção", false),
            ("bad\u{FFFD}", true),
            ("", true),
        ]
    );
}

#[test]
fn encrypted_inputs_need_the_password() {
    let input = fixture("user-password.pdf");
    let outline = read_outline_with_password(&input, Some("user")).unwrap();
    assert!(outline.pages > 0);

    pdq()
        .arg("outline")
        .arg(&input)
        .assert()
        .failure()
        .stderr(predicate::str::contains("--password"));
    pdq()
        .args(["outline", "--password", "user"])
        .arg(&input)
        .assert()
        .success()
        .stdout(predicate::str::starts_with(format!(
            "{{\"pages\":{},\"outline\":",
            outline.pages
        )));
}

#[test]
fn name_tree_cycles_duplicates_and_the_depth_cap_are_handled() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("name-tree.pdf");
    write_pdf(&path, 3, |document, pages| {
        // Two leaves define "dup"; the first one in tree order wins.
        let first = document.add_object(dictionary! {
            "Names" => vec![Object::string_literal("dup"), fit(pages[0])],
        });
        let second = document.add_object(dictionary! {
            "Names" => vec![
                Object::string_literal("dup"),
                fit(pages[1]),
                Object::string_literal("second"),
                fit(pages[1]),
            ],
        });
        // A /Kids chain: "edge" sits at depth 256, the deepest level read, and
        // "deep" at depth 257, past the cap.
        let edge = document.add_object(dictionary! {
            "Names" => vec![Object::string_literal("edge"), fit(pages[2])],
        });
        let deep = document.add_object(dictionary! {
            "Names" => vec![Object::string_literal("deep"), fit(pages[2])],
        });
        let deep_parent =
            document.add_object(dictionary! { "Kids" => vec![Object::Reference(deep)] });
        let mut chain = document.add_object(dictionary! {
            "Kids" => vec![Object::Reference(edge), Object::Reference(deep_parent)],
        });
        for _ in 0..254 {
            chain = document.add_object(dictionary! { "Kids" => vec![Object::Reference(chain)] });
        }
        // The last kid points back to the root: a /Kids cycle.
        let root = document.new_object_id();
        let back = document.add_object(dictionary! { "Kids" => vec![Object::Reference(root)] });
        document.objects.insert(
            root,
            dictionary! {
                "Kids" => vec![
                    Object::Reference(first),
                    Object::Reference(second),
                    Object::Reference(chain),
                    Object::Reference(back),
                ],
            }
            .into(),
        );
        let named = |name: &str| dictionary! { "Dest" => Object::string_literal(name) };
        let outline = add_outline(
            document,
            vec![
                item("Dup", named("dup"), vec![]),
                item("Second", named("second"), vec![]),
                item("Edge", named("edge"), vec![]),
                item("Deep", named("deep"), vec![]),
            ],
        );
        dictionary! { "Outlines" => outline, "Names" => dictionary! { "Dests" => root } }
    });

    assert_eq!(
        pages_by_title(&path),
        vec![
            ("Dup".to_string(), Some(1)),
            ("Second".to_string(), Some(2)),
            ("Edge".to_string(), Some(3)),
            ("Deep".to_string(), None),
        ]
    );
}
