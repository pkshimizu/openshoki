//! 状態を変える唯一の関数（#188 の PR-3b）。
//!
//! **ここが返した `Effect` を、shell は借用を落としてから実行する**。実行の中で別の `Msg` が
//! 起きうるので、`AppState` を借りたまま回すと `BorrowMutError` で常駐プロセスごと落ちる
//! （`docs/rules/coding-conventions.md` の「`RefCell` の借用を持ったまま、作り直しの経路を
//! 呼ばない」）。

use crate::app::{AppState, Job};
use crate::msg::{Command, Effect, Event, Msg};

/// 状態を進めて、shell への依頼を返す。
pub fn update(state: &mut AppState, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Command(command) => update_command(state, command),
        Msg::Event(event) => update_event(state, event),
    }
}

fn update_command(state: &mut AppState, command: Command) -> Vec<Effect> {
    match command {
        Command::Select(None) => {
            state.set_selected(None);
            state.clear_loaded();
            vec![Effect::ClearLoaded]
        }
        Command::Select(Some(dir)) => {
            // **同じ録音を選び直したときは中身を捨てない**（#175）。捨てると、伏せてある途中結果が
            // 1 tick 開いてしまう。別の録音なら捨てる——前の録音の発話を次の録音の画面で読ませない。
            let same = state.selected() == Some(dir.as_path());
            state.set_selected(Some(dir.clone()));
            let mut effects = Vec::new();
            if !same {
                state.clear_loaded();
                effects.push(Effect::ClearLoaded);
            }
            effects.push(Effect::LoadSession {
                dir,
                // 選び直したので音源も差し替える。
                replaces_playback: !same,
            });
            effects
        }
    }
}

fn update_event(state: &mut AppState, event: Event) -> Vec<Effect> {
    match event {
        Event::SessionLoaded {
            dir,
            generation,
            has_readable_segments,
            shortfall,
        } => {
            if state.accept_loaded(dir, generation, has_readable_segments, shortfall) {
                vec![Effect::ShowLoaded]
            } else {
                Vec::new()
            }
        }
        Event::LoadCouldNotStart { dir } => {
            // 読み直せなかった。**「読み込み中」を残さない**——残すと、選び直すまで詳細ペインが
            // その表示のまま固まる。
            if state.selected() == Some(dir.as_path()) {
                state.clear_loaded();
                return vec![Effect::ClearLoaded];
            }
            Vec::new()
        }
        Event::JobChanged { dir, job } => {
            let was_busy = state.job(&dir).is_some_and(|job| job.phase.busy());
            let is_busy = job.as_ref().is_some_and(|job: &Job| job.phase.busy());
            state.set_job(dir.clone(), job);
            // **ワーカーから降りたら読み直す**（#152 / #188）。ここで表示を直接書かないのは、
            // 少し前に始まった読み込みの古いスナップショット（まだ何も無かった頃の中身）が
            // あとから届いて、完成した文字起こしを上書きするため。
            //
            // **選択中のときだけ**。見ていない録音の中身を読み込む理由が無い。
            if was_busy && !is_busy && state.selected() == Some(dir.as_path()) {
                return vec![Effect::LoadSession {
                    dir,
                    // **音は差し替えない**。変わったのは文字起こしだけで、差し替えると鳴っている
                    // 音が止まって先頭へ戻る——文字起こしの完成は再生しながら待つ場面。
                    replaces_playback: false,
                }];
            }
            Vec::new()
        }
        Event::Deleted { dir } => {
            state.set_job(dir.clone(), None);
            if state.selected() == Some(dir.as_path()) {
                state.set_selected(None);
                state.clear_loaded();
                return vec![Effect::ClearLoaded];
            }
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{JobId, JobPhase};
    use crate::reading_pane::TranscriptShortfall;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn running(seq: u64) -> Job {
        Job {
            id: JobId(seq),
            phase: JobPhase::Running {
                model_label: "base".to_owned(),
                percent: None,
            },
        }
    }

    fn done(seq: u64) -> Job {
        Job {
            id: JobId(seq),
            phase: JobPhase::Done { shortfall: None },
        }
    }

    fn loaded(dir: &str, generation: u64) -> Msg {
        Msg::Event(Event::SessionLoaded {
            dir: PathBuf::from(dir),
            generation,
            has_readable_segments: true,
            shortfall: None,
        })
    }

    /// **選び直したら前の中身を捨てる**。捨てないと、読み込みが届くまでの間、前の録音の発話が
    /// 次の録音のヘッダの下に出る（`docs/rules/security.md`）。
    ///
    /// **同じ録音を選び直したときは捨てない**（#175）。捨てると、伏せてある途中結果が 1 tick
    /// 開いてしまう。
    #[test]
    fn choosing_a_different_recording_drops_what_was_shown() {
        let mut state = AppState::default();

        let effects = update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        assert_eq!(
            effects,
            vec![
                Effect::ClearLoaded,
                Effect::LoadSession {
                    dir: dir("a"),
                    replaces_playback: true
                }
            ]
        );

        update(&mut state, loaded("a", 1));

        // 同じ録音をもう一度。中身は捨てない（音も差し替えない）。
        let effects = update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        assert_eq!(
            effects,
            vec![Effect::LoadSession {
                dir: dir("a"),
                replaces_playback: false
            }]
        );

        // 別の録音。捨てる。
        let effects = update(&mut state, Msg::Command(Command::Select(Some(dir("b")))));
        assert_eq!(
            effects,
            vec![
                Effect::ClearLoaded,
                Effect::LoadSession {
                    dir: dir("b"),
                    replaces_playback: true
                }
            ]
        );
    }

    /// **届いた読み込みを受け入れるかは、ここ 1 箇所が決める**（#188）。
    ///
    /// shell 側でもう一度世代を見ると判定が 2 つになり、解除を挟んで世代が飛んだときに
    /// 「core は受け入れたが shell は捨てた」——`AppState` は読み込み済み、画面は空のまま
    /// 「読み込み中」だけが消える——という組み合わせができる。
    #[test]
    fn a_load_is_taken_only_for_the_recording_that_is_selected_now() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));

        // 別の録音の結果は捨てる。
        assert!(update(&mut state, loaded("b", 1)).is_empty());
        // いまの録音なら入れる。
        assert_eq!(update(&mut state, loaded("a", 1)), vec![Effect::ShowLoaded]);
        // **古い世代は捨てる**（速く切り替えると前の読み込みがあとから返る）。
        assert!(update(&mut state, loaded("a", 0)).is_empty());
        // **飛んだ世代は受け入れる**。解除も世代を進めるので、等号だけで見ると正当な結果まで
        // 捨てることになる。
        assert_eq!(update(&mut state, loaded("a", 7)), vec![Effect::ShowLoaded]);
    }

    /// **ワーカーから降りたら読み直す**（#152）。降りたことに気づかないと、文字起こしが
    /// 終わっても本文が出ない。
    ///
    /// **音は差し替えない**。文字起こしの完成は再生しながら待つ場面なので、ここで差し替えると
    /// 鳴っている音が止まって先頭へ戻る。
    #[test]
    fn coming_off_the_worker_reloads_the_selected_recording() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));

        let changed = |job| Msg::Event(Event::JobChanged { dir: dir("a"), job });
        // 走り始めただけでは読み直さない。
        assert!(update(&mut state, changed(Some(running(1)))).is_empty());
        // 降りたら読み直す。
        assert_eq!(
            update(&mut state, changed(Some(done(1)))),
            vec![Effect::LoadSession {
                dir: dir("a"),
                replaces_playback: false
            }]
        );
        // 走っていない状態のまま変わっても読み直さない。
        assert!(update(&mut state, changed(Some(done(1)))).is_empty());
    }

    /// **エントリが消えるのも「降りた」**（止めた・対象が無かった）。相として持たずに消すので、
    /// ここで拾わないと、止めたあとに表示が古いまま残る。
    #[test]
    fn a_job_that_disappears_counts_as_coming_off_the_worker() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        update(
            &mut state,
            Msg::Event(Event::JobChanged {
                dir: dir("a"),
                job: Some(running(1)),
            }),
        );
        assert_eq!(
            update(
                &mut state,
                Msg::Event(Event::JobChanged {
                    dir: dir("a"),
                    job: None,
                }),
            ),
            vec![Effect::LoadSession {
                dir: dir("a"),
                replaces_playback: false
            }]
        );
        assert!(state.job(&dir("a")).is_none());
    }

    /// **選んでいない録音では読み直さない**。見ていない録音の中身を読む理由が無い。
    #[test]
    fn a_job_on_another_recording_does_not_reload() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        update(
            &mut state,
            Msg::Event(Event::JobChanged {
                dir: dir("b"),
                job: Some(running(1)),
            }),
        );
        assert!(
            update(
                &mut state,
                Msg::Event(Event::JobChanged {
                    dir: dir("b"),
                    job: Some(done(1)),
                }),
            )
            .is_empty()
        );
    }

    /// **消した録音の中身は残さない**（`docs/rules/security.md`）。ジョブの記録も落とす——
    /// 残すと、次の tick の差分がゴミ箱へ移した録音を読み直そうとする。
    #[test]
    fn deleting_the_selected_recording_drops_everything_it_left() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        update(&mut state, loaded("a", 1));
        update(
            &mut state,
            Msg::Event(Event::JobChanged {
                dir: dir("a"),
                job: Some(done(1)),
            }),
        );

        assert_eq!(
            update(&mut state, Msg::Event(Event::Deleted { dir: dir("a") })),
            vec![Effect::ClearLoaded]
        );
        assert_eq!(state.selected(), None);
        assert!(state.job(&dir("a")).is_none());
        assert!(state.loaded_for(&dir("a")).is_none());
    }

    /// **読み込みを始められなかったら「読み込み中」を残さない**。残すと、選び直すまで詳細
    /// ペインがその表示のまま固まる（閉じている間に文字起こしが終わり、開き直した直後に
    /// 一覧がまだ空、という順序で起きる）。
    #[test]
    fn a_load_that_could_not_start_does_not_leave_the_pane_loading() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        update(&mut state, loaded("a", 1));
        assert!(!state.is_loading(&dir("a")));

        assert_eq!(
            update(
                &mut state,
                Msg::Event(Event::LoadCouldNotStart { dir: dir("a") })
            ),
            vec![Effect::ClearLoaded]
        );
        assert!(state.is_loading(&dir("a")));
    }

    /// 食い違いは**受け入れた結果からそのまま覚える**（#176）。
    #[test]
    fn what_the_load_found_is_what_the_pane_reads() {
        let mut state = AppState::default();
        update(&mut state, Msg::Command(Command::Select(Some(dir("a")))));
        update(
            &mut state,
            Msg::Event(Event::SessionLoaded {
                dir: dir("a"),
                generation: 1,
                has_readable_segments: true,
                shortfall: Some(TranscriptShortfall::StopsPartway),
            }),
        );
        let facts = state.loaded_for(&dir("a")).expect("the load was taken");
        assert!(facts.has_readable_segments);
        assert_eq!(facts.shortfall, Some(TranscriptShortfall::StopsPartway));
    }
}
