use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use poler_cli::event_plane::{Event, EventClient, EventPlane};

#[test]
fn test_event_plane_and_causal_nexus() {
    let sock_path = "/tmp/test-poler-event.sock";
    let _ = fs::remove_file(sock_path);

    // 1. Запуск EventPlane
    let plane = Arc::new(EventPlane::new(sock_path));
    Arc::clone(&plane).start().expect("Failed to start EventPlane");
    thread::sleep(Duration::from_millis(50));

    // 2. Подключение клиента
    let mut client = EventClient::connect_path(sock_path).expect("Failed to connect EventClient");

    // 3. Тест Causal Nexus NOP-аннигиляции
    let test_file = PathBuf::from("/tmp/poler_causal_test.txt");
    fs::write(&test_file, "hello sovereign core").unwrap();

    let verdict = client
        .check_intent(
            1,
            "touch /tmp/poler_causal_test.txt",
            vec!["touch".into(), "/tmp/poler_causal_test.txt".into()],
            vec!["/tmp/poler_causal_test.txt".into()],
            "/tmp",
        )
        .expect("check_intent failed");

    match verdict {
        Event::NoopCommit { exit_code, reason, .. } => {
            assert_eq!(exit_code, 0);
            assert!(reason.contains("Causal Nexus"));
            println!("✓ Causal Nexus NOP-annihilation verified: {reason}");
        }
        other => panic!("Expected NoopCommit, got {:?}", other),
    }

    // 4. Тест защиты от коллизий (Editor Dirty Buffer)
    let dirty_file = PathBuf::from("/tmp/poler_dirty_file.txt");
    fs::write(&dirty_file, "clean content").unwrap();

    plane
        .registry()
        .register(42, &dirty_file, true, "hash123".into(), 13);

    let collision_verdict = client
        .check_intent(
            2,
            "echo 'hacked' > /tmp/poler_dirty_file.txt",
            vec!["echo".into(), "hacked".into()],
            vec!["/tmp/poler_dirty_file.txt".into()],
            "/tmp",
        )
        .expect("check_intent failed");

    match collision_verdict {
        Event::Block { reason, .. } => {
            assert!(reason.contains("Коллизия"));
            println!("✓ Collision Protection verified: {reason}");
        }
        other => panic!("Expected Block, got {:?}", other),
    }

    // 5. Тест чистого пропуска для независимой команды
    let clean_verdict = client
        .check_intent(
            3,
            "cargo build",
            vec!["cargo".into(), "build".into()],
            vec!["/tmp/new_untracked_binary".into()],
            "/tmp",
        )
        .expect("check_intent failed");

    assert_eq!(clean_verdict, Event::Proceed { id: 3 });
    println!("✓ Clean Proceed verdict verified!");

    // Cleanup
    let _ = fs::remove_file(test_file);
    let _ = fs::remove_file(dirty_file);
    let _ = fs::remove_file(sock_path);
}
