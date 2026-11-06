use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;

const A: &str = "local_aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "local_bbbbbbbb-0000-4000-8000-000000000002";

fn record(id: &str, turns: i64, edge: i64, activity: i64) -> Value {
    json!({
        "sessionId": id, "cliSessionId": format!("cli-{id}"), "lastTurnEdgeAt": edge, "completedTurns": turns,
        "isArchived": false, "createdAt": 1, "lastActivityAt": activity, "title": format!("title of {id}")
    })
}

fn put(dir: &Path, value: &Value) {
    fs::create_dir_all(dir).unwrap();
    let id = value["sessionId"].as_str().unwrap();
    fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(value).unwrap()).unwrap();
}

fn seen(hash: &str, key: Key, born: Option<u128>) -> Seen {
    Seen { hash: hash.into(), key, born }
}

fn state(id: &str, items: &[(&str, Seen)]) -> ProfileState {
    ProfileState { id: id.into(), seen: items.iter().map(|(s, v)| (s.to_string(), v.clone())).collect(), ..ProfileState::default() }
}

/// `(session, origin, [(profile, hash)])`
type Listed<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

fn manifest(items: &[Listed]) -> Manifest {
    let sessions = items
        .iter()
        .map(|(session, origin, copies)| {
            let copies = copies.iter().map(|(p, h)| (p.to_string(), CopyState { hash: h.to_string(), present: true })).collect();
            (session.to_string(), Entry { origin: origin.to_string(), copies, deleted: false })
        })
        .collect();
    Manifest { sessions, ..Manifest::default() }
}

fn write(session: &str, from: &str, to: &str, hash: &str) -> PlannedWrite {
    PlannedWrite { session: session.into(), from: from.into(), to: to.into(), hash: hash.into() }
}

fn action(session: &str, profile: &str) -> Action {
    Action { session: session.into(), profile: profile.into() }
}

mod load {
    use super::*;

    #[test]
    fn load_keeps_valid_records_and_reports_each_skip_without_touching_files() {
        let d = tempfile::tempdir().unwrap();
        put(d.path(), &record(A, 3, 100, 100));
        let bad = d.path().join(format!("{B}.json"));
        fs::write(&bad, b"{ not json").unwrap();
        let missing_id = "local_cccccccc-0000-4000-8000-000000000003";
        let mut missing = record(missing_id, 1, 1, 1);
        missing.as_object_mut().unwrap().remove("cliSessionId");
        put(d.path(), &missing);
        let wrong_name = "local_dddddddd-0000-4000-8000-000000000004";
        fs::write(d.path().join(format!("{wrong_name}.json")), serde_json::to_vec(&record(A, 1, 1, 1)).unwrap()).unwrap();
        fs::write(d.path().join("scheduled-tasks.json"), b"{}").unwrap();
        fs::write(d.path().join(temp_name()), b"partial").unwrap();

        let loaded = load(d.path()).unwrap();
        assert_eq!(loaded.records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec![A]);
        assert_eq!(loaded.records[0].hash, sha256_hex(&loaded.records[0].bytes));
        let skipped: Vec<_> = loaded.skipped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(skipped, vec![B, missing_id, wrong_name]);
        assert!(loaded.skipped[0].reason.contains("not valid JSON"));
        assert!(loaded.skipped[1].reason.contains("cliSessionId"));
        assert!(loaded.skipped[2].reason.contains("sessionId"));
        assert_eq!(fs::read(&bad).unwrap(), b"{ not json");
    }

    #[test]
    fn load_requires_the_documented_types() {
        let d = tempfile::tempdir().unwrap();
        let mut wrong = record(A, 1, 1, 1);
        wrong["isArchived"] = json!("no");
        put(d.path(), &wrong);
        let loaded = load(d.path()).unwrap();
        assert!(loaded.records.is_empty() && loaded.skipped[0].reason.contains("isArchived"));
    }

    #[test]
    fn null_completed_turns_counts_as_zero_and_is_mirrored() {
        let d = tempfile::tempdir().unwrap();
        let mut interrupted = record(A, 0, 5, 5);
        interrupted["completedTurns"] = Value::Null;
        put(d.path(), &interrupted);
        let loaded = load(d.path()).unwrap();
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
        assert_eq!(loaded.records[0].key(), (5, 0, 5));
        assert_eq!(loaded.records[0].bytes, serde_json::to_vec(&interrupted).unwrap());

        let profiles = [state("personal", &[(A, seen(&loaded.records[0].hash, loaded.records[0].key(), Some(1)))]), state("team", &[])];
        let p = plan(&Manifest::default(), &profiles, Some("personal"));
        assert_eq!(p.writes, vec![write(A, "personal", "team", &loaded.records[0].hash)]);
    }

    #[test]
    fn missing_completed_turns_counts_as_zero_and_is_mirrored() {
        let d = tempfile::tempdir().unwrap();
        let mut crashed = record(A, 0, 5, 5);
        crashed.as_object_mut().unwrap().remove("completedTurns");
        put(d.path(), &crashed);
        let loaded = load(d.path()).unwrap();
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
        assert_eq!(loaded.records[0].key(), (5, 0, 5));
        assert_eq!(loaded.records[0].bytes, serde_json::to_vec(&crashed).unwrap());
    }

    #[test]
    fn string_completed_turns_is_still_skipped() {
        let d = tempfile::tempdir().unwrap();
        let mut wrong = record(A, 1, 1, 1);
        wrong["completedTurns"] = json!("3");
        put(d.path(), &wrong);
        let loaded = load(d.path()).unwrap();
        assert!(loaded.records.is_empty() && loaded.skipped[0].reason.contains("completedTurns"));
    }

    #[test]
    fn load_session_dir_needs_exactly_one_organization_folder() {
        let d = tempfile::tempdir().unwrap();
        assert!(session_dir(d.path(), "acct").is_err());
        let acct = d.path().join(SESSIONS).join("acct");
        fs::create_dir_all(&acct).unwrap();
        assert!(session_dir(d.path(), "acct").unwrap_err().contains("no organization"));
        fs::create_dir_all(acct.join("org1")).unwrap();
        assert_eq!(session_dir(d.path(), "acct").unwrap(), acct.join("org1"));
        fs::create_dir_all(acct.join("org2")).unwrap();
        assert!(session_dir(d.path(), "acct").unwrap_err().contains("2 organization"));
    }
}

mod plan {
    use super::*;

    #[test]
    fn new_session_reaches_all_profiles() {
        let profiles = [state("personal", &[(A, seen("h1", (10, 2, 10), Some(1)))]), state("team", &[]), state("third", &[])];
        let p = plan(&Manifest::default(), &profiles, Some("personal"));
        assert_eq!(p.writes, vec![write(A, "personal", "team", "h1"), write(A, "personal", "third", "h1")]);
        assert!(p.backups.is_empty() && p.moves.is_empty());
        let m = p.manifest.unwrap();
        assert_eq!(m.sessions[A].origin, "personal");
        assert!(m.sessions[A].copies.values().all(|c| c.hash == "h1" && c.present));
        assert_eq!(m.sessions[A].copies.len(), 3);
    }

    #[test]
    fn single_change_wins() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        let profiles = [state("personal", &[(A, seen("h1", (10, 2, 99), Some(1)))]), state("team", &[(A, seen("h0", (10, 2, 10), Some(2)))])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.writes, vec![write(A, "personal", "team", "h1")]);
        assert!(p.backups.is_empty(), "an unchanged copy is not a loser");
        assert_eq!(p.manifest.unwrap().sessions[A].copies["team"].hash, "h1");
    }

    #[test]
    fn conflict_more_turns_wins_and_loser_backed_up() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        let profiles = [state("personal", &[(A, seen("hp", (20, 3, 20), None))]), state("team", &[(A, seen("ht", (30, 4, 30), None))])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.writes, vec![write(A, "team", "personal", "ht")]);
        assert_eq!(p.backups, vec![action(A, "personal")]);
    }

    #[test]
    fn focus_only_change_cannot_beat_more_turns() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        // Personal was only viewed (newest activity, same turns); team gained a turn.
        let profiles = [state("personal", &[(A, seen("hp", (20, 3, 9_999), None))]), state("team", &[(A, seen("ht", (30, 4, 31), None))])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.writes, vec![write(A, "team", "personal", "ht")]);
        assert_eq!(p.backups, vec![action(A, "personal")]);
    }

    #[test]
    fn archive_carries_across() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        let profiles = [state("personal", &[(A, seen("h0", (20, 3, 20), None))]), state("team", &[(A, seen("archived", (20, 3, 20), None))])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.writes, vec![write(A, "team", "personal", "archived")]);
        assert!(p.backups.is_empty());
    }

    #[test]
    fn delete_moves_other_copies_to_backup() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")]), (B, "personal", &[("personal", "k0"), ("team", "k0")])]);
        let keep = seen("k0", (1, 1, 1), None);
        let profiles = [state("personal", &[(B, keep.clone())]), state("team", &[(A, seen("h0", (20, 3, 20), None)), (B, keep)])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.moves, vec![action(A, "team")]);
        assert!(p.writes.is_empty() && p.backups.is_empty());
        assert!(!p.manifest.unwrap().sessions.contains_key(A), "the entry is dropped");
    }

    #[test]
    fn account_change_in_profile_is_not_a_deletion() {
        let mut m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        m.profiles = [("personal".to_string(), "acct1/org1".to_string()), ("team".to_string(), "acct2/org2".to_string())].into();
        let mut personal = state("personal", &[(A, seen("h0", (20, 3, 20), None))]);
        personal.folder = "acct1/org1".into();
        let mut team = state("team", &[]);
        team.folder = "acct3/org3".into(); // another account signed in: its folder is empty
        let p = plan(&m, &[personal, team], None);
        assert!(p.moves.is_empty() && p.writes.is_empty() && p.backups.is_empty());
        let next = p.manifest.unwrap();
        assert_eq!(next.sessions[A], m.sessions[A], "the old copy states are kept");
        assert_eq!(next.profiles["team"], "acct2/org2");
        assert!(p.notes.iter().any(|n| n.contains("team") && n.contains("another account")), "{:?}", p.notes);
    }

    #[test]
    fn wiped_profile_does_not_propagate_deletes() {
        let ids = ["local_s1", "local_s2", "local_s3", "local_s4"];
        let copies: &[(&str, &str)] = &[("personal", "h"), ("team", "h")];
        let m = manifest(&ids.iter().map(|id| (*id, "personal", copies)).collect::<Vec<_>>());
        let held: Vec<(&str, Seen)> = ids.iter().map(|id| (*id, seen("h", (1, 1, 1), None))).collect();
        let p = plan(&m, &[state("personal", &held), state("team", &[])], None);
        assert!(p.moves.is_empty() && p.writes.is_empty());
        assert_eq!(p.manifest.unwrap().sessions, m.sessions);
        assert!(p.notes.iter().any(|n| n.contains("team lost 4 sessions at once; deletion not propagated")), "{:?}", p.notes);

        // Three of four gone at once (more than two and over a quarter) is also held back.
        let team: Vec<(&str, Seen)> = vec![(ids[3], seen("h", (1, 1, 1), None))];
        let p = plan(&m, &[state("personal", &held), state("team", &team)], None);
        assert!(p.moves.is_empty());

        // One of four is an ordinary deletion.
        let team: Vec<(&str, Seen)> = ids[1..].iter().map(|id| (*id, seen("h", (1, 1, 1), None))).collect();
        let p = plan(&m, &[state("personal", &held), state("team", &team)], None);
        assert_eq!(p.moves, vec![action(ids[0], "personal")]);
    }

    #[test]
    fn deletion_with_skipped_profile_keeps_tombstone() {
        let m = manifest(&[
            (A, "personal", &[("personal", "h0"), ("team", "h0"), ("third", "h0")]),
            (B, "personal", &[("personal", "k0"), ("team", "k0"), ("third", "k0")]),
        ]);
        let keep = seen("k0", (1, 1, 1), None);
        // `third` is not readable this run. A was deleted in personal; team still has it.
        let profiles = [state("personal", &[(B, keep.clone())]), state("team", &[(A, seen("h0", (2, 2, 2), None)), (B, keep.clone())])];
        let p = plan(&m, &profiles, None);
        assert_eq!(p.moves, vec![action(A, "team")]);
        let next = p.manifest.unwrap();
        assert!(next.sessions[A].deleted);
        assert_eq!(next.sessions[A].copies.keys().collect::<Vec<_>>(), vec!["third"]);

        // Next run `third` is readable and still holds the copy: it is moved too, and the tombstone goes.
        let profiles = [state("personal", &[(B, keep.clone())]), state("team", &[(B, keep.clone())]), state("third", &[(A, seen("h0", (2, 2, 2), None)), (B, keep)])];
        let again = plan(&next, &profiles, None);
        assert_eq!(again.moves, vec![action(A, "third")]);
        assert!(!again.manifest.unwrap().sessions.contains_key(A));
        assert!(again.writes.iter().all(|w| w.session != A), "a deleted session is never resurrected");
    }

    #[test]
    fn unknown_session_is_never_deleted() {
        // No manifest entry: absent in one profile is "new elsewhere", never "deleted".
        let m = manifest(&[(B, "personal", &[("personal", "x"), ("team", "x")])]);
        let profiles = [state("personal", &[]), state("team", &[(A, seen("h1", (1, 1, 1), Some(1)))])];
        let p = plan(&m, &profiles, None);
        assert!(p.moves.iter().all(|a| a.session != A));
        assert_eq!(p.writes, vec![write(A, "team", "personal", "h1")]);
    }

    #[test]
    fn bootstrap_spike_layout_team_seven_turns_wins_origin_personal() {
        let profiles = [state("personal", &[(A, seen("six", (100, 6, 100), Some(1_000)))]), state("team", &[(A, seen("seven", (200, 7, 300), Some(2_000)))])];
        let p = plan(&Manifest::default(), &profiles, Some("team"));
        assert_eq!(p.writes, vec![write(A, "team", "personal", "seven")]);
        assert_eq!(p.backups, vec![action(A, "personal")]);
        assert_eq!(p.manifest.unwrap().sessions[A].origin, "personal", "earliest birth time, not the default profile");
    }

    #[test]
    fn an_invalid_file_is_never_written_over_or_counted_as_deleted() {
        let m = manifest(&[(A, "personal", &[("personal", "h0"), ("team", "h0")])]);
        let mut team = state("team", &[]);
        team.blocked.insert(A.to_string());
        let profiles = [state("personal", &[(A, seen("h1", (20, 3, 20), None))]), team];
        let p = plan(&m, &profiles, None);
        assert!(p.writes.is_empty() && p.moves.is_empty() && p.backups.is_empty());
    }
}

mod execute {
    use super::*;

    struct Fx {
        _root: tempfile::TempDir,
        layout: Layout,
    }

    fn fixture() -> Fx {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("p/org");
        let t = root.path().join("t/org");
        fs::create_dir_all(&p).unwrap();
        fs::create_dir_all(&t).unwrap();
        let layout = Layout {
            dirs: [("personal".to_string(), p), ("team".to_string(), t)].into(),
            backup_root: root.path().join("backups/sessions/20261106-000000"),
            manifest_file: root.path().join("claude-desktop/session-mirror.json"),
        };
        Fx { _root: root, layout }
    }

    fn states(layout: &Layout) -> Vec<ProfileState> {
        layout
            .dirs
            .iter()
            .map(|(id, dir)| {
                let loaded = load(dir).unwrap();
                Scanned { profile: ProfileRef { id: id.clone(), dir: dir.clone(), is_default: false }, dir: dir.clone(), loaded }.state()
            })
            .collect()
    }

    fn file(layout: &Layout, profile: &str, session: &str) -> PathBuf {
        layout.dirs[profile].join(format!("{session}.json"))
    }

    #[test]
    fn rerun_after_crash_is_idempotent() {
        let fx = fixture();
        put(&fx.layout.dirs["personal"], &record(A, 6, 100, 100));
        put(&fx.layout.dirs["team"], &record(A, 7, 200, 200));
        let winner = fs::read(file(&fx.layout, "team", A)).unwrap();

        let first = plan(&Manifest::default(), &states(&fx.layout), None);
        let stats = execute(&first, &fx.layout, || false).unwrap();
        assert_eq!(stats, Stats { mirrored: 1, backed_up: 1 });
        assert_eq!(fs::read(file(&fx.layout, "personal", A)).unwrap(), winner, "bytes are copied unchanged");
        assert_eq!(fs::metadata(file(&fx.layout, "personal", A)).unwrap().permissions().mode() & 0o777, 0o600);
        let backup = fx.layout.backup_root.join("personal").join(format!("{A}.json"));
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&backup).unwrap()).unwrap()["completedTurns"], 6);

        // Crash after the writes, before the manifest: it is lost. The next run reaches the same result.
        fs::remove_file(&fx.layout.manifest_file).unwrap();
        let second = plan(&Manifest::default(), &states(&fx.layout), None);
        assert!(second.writes.is_empty() && second.backups.is_empty() && second.moves.is_empty());
        assert_eq!(second.manifest.as_ref().unwrap().sessions[A].copies["team"].hash, sha256_hex(&winner));
        execute(&second, &fx.layout, || false).unwrap();
        assert_eq!(Manifest::load(&fx.layout.manifest_file).unwrap().sessions[A].copies["personal"].hash, sha256_hex(&winner));
        assert_eq!(fs::read(file(&fx.layout, "personal", A)).unwrap(), winner);
    }

    #[test]
    fn no_write_while_running() {
        let fx = fixture();
        put(&fx.layout.dirs["personal"], &record(A, 6, 100, 100));
        let p = plan(&Manifest::default(), &states(&fx.layout), None);
        assert_eq!(p.writes.len(), 1);
        let err = execute(&p, &fx.layout, || true).unwrap_err().to_string();
        assert!(err.contains("Claude is running"), "{err}");
        assert!(!file(&fx.layout, "team", A).exists());
        assert!(!fx.layout.manifest_file.exists() && !fx.layout.backup_root.exists());
    }

    #[test]
    fn temp_name_never_matches_local_glob() {
        let name = temp_name();
        assert!(name.starts_with(".agent-switch-") && name.ends_with(".tmp"), "{name}");
        assert!(!name.starts_with("local_") && !name.ends_with(".json"));
        assert_ne!(name, temp_name());

        // A leftover temp file is ignored by `load`, and a normal write leaves none behind.
        let fx = fixture();
        put(&fx.layout.dirs["personal"], &record(A, 1, 1, 1));
        fs::write(fx.layout.dirs["personal"].join(&name), b"{}").unwrap();
        execute(&plan(&Manifest::default(), &states(&fx.layout), None), &fx.layout, || false).unwrap();
        assert_eq!(load(&fx.layout.dirs["personal"]).unwrap().records.len(), 1);
        let leftovers = fs::read_dir(&fx.layout.dirs["team"]).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn execute_moves_deleted_sessions_into_the_backup_folder_instead_of_unlinking() {
        let fx = fixture();
        put(&fx.layout.dirs["team"], &record(A, 2, 5, 5));
        // Personal keeps B, so losing A alone is a deletion and not a wipe.
        put(&fx.layout.dirs["personal"], &record(B, 1, 1, 1));
        put(&fx.layout.dirs["team"], &record(B, 1, 1, 1));
        let hash = |profile: &str, id: &str| sha256_hex(&fs::read(file(&fx.layout, profile, id)).unwrap());
        let m = manifest(&[
            (A, "personal", &[("personal", "gone"), ("team", &hash("team", A))]),
            (B, "personal", &[("personal", &hash("personal", B)), ("team", &hash("team", B))]),
        ]);
        let p = plan(&m, &states(&fx.layout), None);
        let stats = execute(&p, &fx.layout, || false).unwrap();
        assert_eq!(stats, Stats { mirrored: 0, backed_up: 1 });
        assert!(!file(&fx.layout, "team", A).exists());
        assert!(fx.layout.backup_root.join("team").join(format!("{A}.json")).exists());
        let saved = Manifest::load(&fx.layout.manifest_file).unwrap();
        assert!(!saved.sessions.contains_key(A) && saved.sessions.contains_key(B));
    }

    #[test]
    fn second_holder_is_refused_cleanly() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_home(home.path());
        let held = lock(&paths).unwrap();
        assert!(matches!(sync(&paths, || false), Err(Error::LockBusy(_))));
        assert!(matches!(mirror_on(&paths), Err(Error::LockBusy(_))));
        drop(held);
        assert!(sync(&paths, || false).is_ok());
    }

    #[test]
    fn execute_refuses_a_source_that_changed_since_the_plan() {
        let fx = fixture();
        put(&fx.layout.dirs["personal"], &record(A, 1, 1, 1));
        let p = plan(&Manifest::default(), &states(&fx.layout), None);
        put(&fx.layout.dirs["personal"], &record(A, 2, 2, 2));
        assert!(execute(&p, &fx.layout, || false).is_err());
        assert!(!file(&fx.layout, "team", A).exists());
    }
}

mod busy {
    use super::*;

    const UUID: &str = "acct-uuid";

    fn profile_with(value: &Value) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join(SESSIONS).join(UUID).join("org");
        put(&dir, value);
        (d, dir)
    }

    #[test]
    fn idle_records_are_not_busy() {
        let mut idle = record(A, 3, 100, 500);
        idle["latestUserFrameAt"] = json!(100);
        idle["interruptedAwaitingPerson"] = Value::Null;
        let (d, _) = profile_with(&idle);
        assert!(busy_sessions(d.path(), UUID).unwrap().is_empty());
        assert!(busy_sessions(d.path(), "other-account").unwrap().is_empty(), "no session folder means nothing is busy");
    }

    #[test]
    fn user_frame_after_edge_is_busy() {
        let mut v = record(A, 3, 100, 500);
        v["latestUserFrameAt"] = json!(101);
        let (d, _) = profile_with(&v);
        assert_eq!(busy_sessions(d.path(), UUID).unwrap(), vec![format!("title of {A}")]);
    }

    #[test]
    fn awaiting_person_is_busy() {
        let mut v = record(A, 3, 100, 500);
        v["interruptedAwaitingPerson"] = json!({"tool": "Bash"});
        v["title"] = json!("");
        let (d, _) = profile_with(&v);
        assert_eq!(busy_sessions(d.path(), UUID).unwrap(), vec![A.to_string()], "the session id stands in for an empty title");
    }

    #[test]
    fn dotfile_in_waiting_input_is_not_busy() {
        let (d, dir) = profile_with(&record(A, 3, 100, 500));
        fs::create_dir_all(dir.join("waiting-input")).unwrap();
        fs::write(dir.join("waiting-input/.DS_Store"), b"x").unwrap();
        assert!(busy_sessions(d.path(), UUID).unwrap().is_empty());
    }

    #[test]
    fn waiting_input_file_is_busy() {
        let (d, dir) = profile_with(&record(A, 3, 100, 500));
        fs::create_dir_all(dir.join("waiting-input")).unwrap();
        assert!(busy_sessions(d.path(), UUID).unwrap().is_empty(), "an empty folder is not a request");
        fs::write(dir.join("waiting-input/request.json"), b"{}").unwrap();
        let busy = busy_sessions(d.path(), UUID).unwrap();
        assert_eq!(busy.len(), 1);
        assert!(busy[0].contains("waiting for your input"), "{busy:?}");
    }
}

#[test]
fn busy_refusal_text_yields_its_titles_and_other_errors_do_not() {
    let text = format!("{BUSY_PREFIX}Fix login, Refactor. Nothing was quit. Wait for it to finish, or add --force");
    assert_eq!(busy_titles_in(&text).as_deref(), Some("Fix login, Refactor"));
    assert_eq!(busy_titles_in("Claude is running (pid 1). Switching needs every instance closed"), None);
}
