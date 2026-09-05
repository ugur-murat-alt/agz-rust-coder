"""Apply after repair_portable_contracts.py; keep production limits unchanged."""
from pathlib import Path


def replace(name, old, new):
    p = Path(name)
    text = p.read_text(encoding='utf-8')
    assert text.count(old) == 1, (name, old, text.count(old))
    p.write_text(text.replace(old, new), encoding='utf-8', newline='\n')


replace('crates/agz-rust-coder/src/workspace/metadata.rs',
'''            let guard = lock.lock().expect("deadline lock");
            let _ = Condvar::new()
                .wait_timeout(
                    guard,
                    control.deadline.saturating_duration_since(Instant::now()),
                )
                .expect("deadline wait");''',
'''            let mut guard = lock.lock().expect("deadline lock");
            let ready = Condvar::new();
            // The mock must return late, even after a spurious or rounded wake.
            while Instant::now() < control.deadline {
                let (next, _) = ready
                    .wait_timeout(
                        guard,
                        control.deadline.saturating_duration_since(Instant::now()),
                    )
                    .expect("deadline wait");
                guard = next;
            }''')
name = 'crates/agz-rust-coder/tests/protocol_tasks.rs'
replace(name, '        "agz-rust-coder-docs-task-{}-{stamp}",', '        "agz-d-{}-{stamp:x}",')
replace(name, '        "agz-rust-coder-docs-task-cache-{}-{stamp}",', '        "agz-c-{}-{stamp:x}",')
replace(name, '    let root = temp.join(format!(\n        "agz-d-{}-{stamp:x}",',
'''    // This test checks cancellation, not the linker's maximum output path.
    // Leave room for the cache digest and Cargo's build-script subdirectories.
    let root = temp.join(format!(
        "agz-d-{}-{stamp:x}",''')
print('Applied fixture-only deadline and path bounds; production policies unchanged.')
