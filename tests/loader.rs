//! R4 loader integration: drive the SINGLE worker thread over a real on-disk
//! repository through the public surface only.
//!
//! It builds a throwaway git repo in a temp dir with `git2`, opens it via the
//! crate's `open_real_backend` composition root, spawns the loader, and asserts
//! the off-thread pipeline: the INITIAL `RepoLoaded` arrives with the tempdir
//! commit, and a `Req::OpenFile` round-trips to a `PreviewLoaded` for that file.
//! All over `mpsc` with a timeout, so a hang fails instead of blocking forever.

use std::sync::mpsc;
use std::time::Duration;

use git2::{Repository, Signature, Time};
use gitgit::loader::{spawn_loader, Req};
use gitgit::message::Msg;
use gitgit::model::{FileStatus, TreeNode, WORKING_REV};

/// Receive the next message, failing the test if none arrives within `RECV_TIMEOUT`.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// A unique temp dir removed on drop, so the test repo stays isolated.
struct TempRepo {
    dir: std::path::PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gitgit-loader-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        TempRepo { dir }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Build a one-commit repo adding `a.go`, returning its temp dir.
fn build_repo() -> TempRepo {
    let tmp = TempRepo::new();
    let repo = Repository::init(&tmp.dir).unwrap();
    std::fs::write(tmp.dir.join("a.go"), "package main\n\nfunc A() {}\n").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = Signature::new("Test Author", "test@example.com", &Time::new(1_779_451_680, 0)).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "root: add a.go", &tree, &[])
        .unwrap();
    tmp
}

/// Build a TWO-commit repo: c1 adds `a.go` + `keep.txt`; c2 modifies `a.go` and
/// adds `b.go` (keep.txt is untouched). Returns the temp dir + HEAD's short hash.
fn build_two_commit_repo() -> (TempRepo, String) {
    let tmp = TempRepo::new();
    let repo = Repository::init(&tmp.dir).unwrap();
    let sig = Signature::new("Test Author", "test@example.com", &Time::new(1_779_451_680, 0)).unwrap();
    let commit_all = |msg: &str, parent: Option<git2::Oid>| -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parents: Vec<git2::Commit> = parent.map(|p| repo.find_commit(p).unwrap()).into_iter().collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &refs).unwrap()
    };
    std::fs::write(tmp.dir.join("a.go"), "package main\n\nfunc A() {}\n").unwrap();
    std::fs::write(tmp.dir.join("keep.txt"), "unchanged\n").unwrap();
    let c1 = commit_all("root", None);
    std::fs::write(tmp.dir.join("a.go"), "package main\n\nfunc A() { return }\n").unwrap();
    std::fs::write(tmp.dir.join("b.go"), "package main\n\nfunc B() {}\n").unwrap();
    let c2 = commit_all("second", Some(c1));
    (tmp, c2.to_string().chars().take(8).collect())
}

/// Recursively collect a loaded TreeNode tree into (file name, status) pairs,
/// regardless of each directory's expand state.
fn collect_files(nodes: &[TreeNode]) -> Vec<(String, FileStatus)> {
    let mut out = Vec::new();
    for node in nodes {
        match node {
            TreeNode::Dir { children, .. } => out.extend(collect_files(children)),
            TreeNode::File { name, status } => out.push((name.clone(), *status)),
        }
    }
    out
}

/// Receive with a timeout, panicking on a hang so the test never blocks forever.
fn recv(rx: &mpsc::Receiver<Msg>) -> Msg {
    rx.recv_timeout(RECV_TIMEOUT).expect("loader reply within timeout")
}

#[test]
fn loader_initial_load_then_open_file_round_trip() {
    let tmp = build_repo();
    let backend = gitgit::open_real_backend(&tmp.dir, &gitgit::config::Config::default())
        .expect("open temp repo");

    let (tx, rx) = mpsc::channel::<Msg>();
    let req_tx = spawn_loader(backend, tx);

    // 1) The INITIAL load: the worker pushes RepoLoaded with the tempdir commit.
    let model = match recv(&rx) {
        Msg::RepoLoaded(model) => model,
        other => panic!("expected RepoLoaded first, got {other:?}"),
    };
    // A clean tree still pins the "<current>  no changes" row at index 0, so the lone
    // real commit is row 1.
    assert_eq!(model.commits.len(), 2, "the <current> row + the tempdir's single commit");
    assert!(model.commits[0].is_working, "row 0 is the pinned <current> row");
    assert_eq!(model.commits[1].author, "Test Author");
    let commit = model.commits[1].hash.clone();

    // 2) OpenFile on the <current> working row -> EDITABLE (base = HEAD blob, work = the
    //    worktree; the clean tree makes them equal). Only the working row is editable.
    req_tx
        .send(Req::OpenFile {
            commit: WORKING_REV.to_string(),
            path: "a.go".to_string(),
        })
        .unwrap();
    match recv(&rx) {
        Msg::EditFileLoaded { commit: c, path, base, work } => {
            assert_eq!(c, WORKING_REV);
            assert_eq!(path, "a.go");
            assert_eq!(work, "package main\n\nfunc A() {}\n");
            assert_eq!(base.as_deref(), Some("package main\n\nfunc A() {}\n"));
        }
        other => panic!("expected EditFileLoaded, got {other:?}"),
    }

    // 3) OpenFile on a real (historical) commit -> READ-ONLY parent-vs-commit preview, not
    //    an editable buffer (the right side is that commit's blob, not the working tree).
    req_tx
        .send(Req::OpenFile { commit: commit.clone(), path: "a.go".to_string() })
        .unwrap();
    match recv(&rx) {
        Msg::PreviewLoaded { commit: c, path, .. } => {
            assert_eq!(c, commit);
            assert_eq!(path, "a.go");
        }
        other => panic!("a historical commit must round-trip to a read-only PreviewLoaded, got {other:?}"),
    }
}

#[test]
fn loader_editor_open_then_save_round_trip() {
    // The editable-diff seam end-to-end over the worker thread: OpenFile returns the
    // base + working text, SaveFile writes the edit back, and a re-read sees it.
    let (tmp, _head) = build_two_commit_repo();
    let dir = tmp.dir.clone();
    let backend = gitgit::open_real_backend(&tmp.dir, &gitgit::config::Config::default())
        .expect("open temp repo");
    let (tx, rx) = mpsc::channel::<Msg>();
    let req_tx = spawn_loader(backend, tx);

    // Drain the INITIAL RepoLoaded.
    match recv(&rx) {
        Msg::RepoLoaded(_) => {}
        other => panic!("expected RepoLoaded first, got {other:?}"),
    }

    // OpenFile on the <current> working row -> editable base + working text of a.go (a
    // clean worktree at c2, so base == work == c2's content). The editable seam is the
    // working row; a historical commit would round-trip to a read-only PreviewLoaded.
    req_tx.send(Req::OpenFile { commit: WORKING_REV.to_string(), path: "a.go".to_string() }).unwrap();
    match recv(&rx) {
        Msg::EditFileLoaded { path, base, work, .. } => {
            assert_eq!(path, "a.go");
            assert_eq!(work, "package main\n\nfunc A() { return }\n");
            assert_eq!(base.as_deref(), Some("package main\n\nfunc A() { return }\n"), "committed base");
        }
        other => panic!("expected EditFileLoaded, got {other:?}"),
    }

    // SaveFile -> writes the edited content; FileSaved confirms.
    let edited = "package main\n\nfunc A() { return }\n";
    req_tx
        .send(Req::SaveFile { path: "a.go".to_string(), content: edited.to_string() })
        .unwrap();
    match recv(&rx) {
        Msg::FileSaved { path } => assert_eq!(path, "a.go"),
        other => panic!("expected FileSaved, got {other:?}"),
    }
    // The on-disk file holds the saved content.
    assert_eq!(std::fs::read_to_string(dir.join("a.go")).unwrap(), edited);
}

#[test]
fn loader_revert_hunk_round_trip() {
    // The hunk-revert seam over the worker: c2 changes a.go; reverting hunk 0 returns
    // the working-tree file to the parent's content, and HunkReverted confirms.
    let (tmp, head) = build_two_commit_repo();
    let dir = tmp.dir.clone();
    let backend = gitgit::open_real_backend(&tmp.dir, &gitgit::config::Config::default())
        .expect("open temp repo");
    let (tx, rx) = mpsc::channel::<Msg>();
    let req_tx = spawn_loader(backend, tx);
    match recv(&rx) {
        Msg::RepoLoaded(_) => {}
        other => panic!("expected RepoLoaded first, got {other:?}"),
    }

    req_tx
        .send(Req::RevertHunk { commit: head, path: "a.go".to_string(), hunk: 0 })
        .unwrap();
    match recv(&rx) {
        Msg::HunkReverted { summary } => assert!(summary.contains("a.go")),
        other => panic!("expected HunkReverted, got {other:?}"),
    }
    // a.go's sole change is reverted -> back to c1's content.
    assert_eq!(std::fs::read_to_string(dir.join("a.go")).unwrap(), "package main\n\nfunc A() {}\n");
}

#[test]
fn loader_tree_request_serves_changed_vs_full_by_mode() {
    let (tmp, head) = build_two_commit_repo();
    let backend = gitgit::open_real_backend(&tmp.dir, &gitgit::config::Config::default())
        .expect("open temp repo");
    let (tx, rx) = mpsc::channel::<Msg>();
    let req_tx = spawn_loader(backend, tx);

    // Drain the INITIAL RepoLoaded.
    match recv(&rx) {
        Msg::RepoLoaded(_) => {}
        other => panic!("expected RepoLoaded first, got {other:?}"),
    }

    // all=false -> the CHANGED-only tree: a.go (Modified) + b.go (Added), no keep.txt.
    req_tx.send(Req::Tree { hash: head.clone(), all: false }).unwrap();
    let changed = match recv(&rx) {
        Msg::TreeLoaded { tree, .. } => collect_files(&tree),
        other => panic!("expected TreeLoaded (changed), got {other:?}"),
    };
    assert!(
        changed.iter().any(|(n, s)| n == "a.go" && *s == FileStatus::Modified),
        "changed tree has a.go Modified"
    );
    assert!(!changed.iter().any(|(n, _)| n == "keep.txt"), "changed tree omits the unchanged file");

    // all=true -> the FULL tree: every file, with keep.txt marked Unchanged.
    req_tx.send(Req::Tree { hash: head.clone(), all: true }).unwrap();
    let full = match recv(&rx) {
        Msg::TreeLoaded { tree, .. } => collect_files(&tree),
        other => panic!("expected TreeLoaded (full), got {other:?}"),
    };
    let status_of = |name: &str| full.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
    assert_eq!(status_of("a.go"), Some(FileStatus::Modified), "full tree keeps a.go Modified");
    assert_eq!(status_of("b.go"), Some(FileStatus::Added), "full tree keeps b.go Added");
    assert_eq!(status_of("keep.txt"), Some(FileStatus::Unchanged), "full tree adds keep.txt Unchanged");
    assert!(full.len() > changed.len(), "the full tree shows more files than the changed tree");
}
