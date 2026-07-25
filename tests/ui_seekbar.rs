//! シークバー（`ui/recordings-window.slint` の `SeekBar`）の操作を、実際のポインタイベントで
//! 検証する。ビルド・clippy・純粋関数の単体テストでは「TouchArea の配線」を検証できないため、
//! Slint のテストバックエンドでクリック・ドラッグを流して振る舞いを固定する。
//!
//! 見た目（レイアウト・配色）の確認は `examples/transcript_view.rs` ＋ screencapture
//! （`docs/rules/slint.md`）。こちらは「操作したら何が起きるか」を担当する。
//!
//! 前提: 要素を探す `ElementHandle` は生成コードのデバッグ情報を要求する。`build.rs` が
//! dev/test プロファイルでのみ有効にしているため、素の `cargo test` で通る（環境変数は不要）。

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;
use slint::LogicalPosition;
use slint::platform::PointerEventButton;

/// テスト用のウィンドウサイズ（`src/main.rs` の RECORDINGS_WIDTH/HEIGHT と同じ）。要素の座標は
/// レイアウトから解決するので、この値自体をアサートには使わない。
const WINDOW_WIDTH: f32 = 720.0;
const WINDOW_HEIGHT: f32 = 540.0;

/// 比率の許容誤差。ポインタ座標は論理ピクセル単位なので、バー幅ぶんの丸めを吸収する。
const RATIO_TOLERANCE: f32 = 0.01;

/// テストバックエンドはスレッドごとに 1 回だけ初期化する（2 回目は panic する）。
/// `--test-threads=1` で複数のテストが同じスレッドに載っても壊れないようにする。
fn init_backend() {
    thread_local! {
        static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }
    INITIALIZED.with(|initialized| {
        if !initialized.replace(true) {
            i_slint_backend_testing::init_no_event_loop();
        }
    });
}

/// シークバーを操作できる状態の Recordings ウィンドウと、コールバックの記録。
struct Probe {
    window: RecordingsWindow,
    /// `scrub-preview` で渡された比率（ドラッグ中のプレビュー）。
    previews: Rc<RefCell<Vec<f32>>>,
    /// プレビューが来た時点の `scrubbing`（Rust 側の tick 抑止が効く前提の確認）。
    scrubbing_at_preview: Rc<RefCell<Vec<bool>>>,
    /// `seek-to-ratio` で渡された比率（クリック確定・ドラッグ終了）。
    seeks: Rc<RefCell<Vec<f32>>>,
}

impl Probe {
    fn new(seekable: bool) -> Self {
        init_backend();
        let window = RecordingsWindow::new().expect("creating the window should succeed");
        window
            .window()
            .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
        // シークバーは詳細ペイン（`has-selection` の内側）にあるので、選択済みの状態にする。
        window.set_has_selection(true);
        window.set_playable(true);
        window.set_seekable(seekable);

        let previews: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
        let scrubbing_at_preview: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let seeks: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let previews = Rc::clone(&previews);
            let scrubbing_at_preview = Rc::clone(&scrubbing_at_preview);
            let weak = window.as_weak();
            window.on_scrub_preview(move |ratio| {
                previews.borrow_mut().push(ratio);
                let scrubbing = weak
                    .upgrade()
                    .expect("the window outlives the callback in this test")
                    .get_scrubbing();
                scrubbing_at_preview.borrow_mut().push(scrubbing);
            });
        }
        {
            let seeks = Rc::clone(&seeks);
            window.on_seek_to_ratio(move |ratio| seeks.borrow_mut().push(ratio));
        }

        Self {
            window,
            previews,
            scrubbing_at_preview,
            seeks,
        }
    }

    /// シークバー本体。座標はレイアウト結果から取るので、ウィンドウ構成が変わっても追従する。
    fn bar(&self) -> ElementHandle {
        ElementHandle::find_by_element_type_name(&self.window, "SeekBar")
            .next()
            .expect("the Playback section contains a SeekBar")
    }

    /// バー上の比率に対応する絶対座標（縦はバーの中央）。
    fn point_at(&self, ratio: f32) -> LogicalPosition {
        let bar = self.bar();
        let position = bar.absolute_position();
        let size = bar.size();
        LogicalPosition::new(
            position.x + size.width * ratio,
            position.y + size.height / 2.0,
        )
    }

    fn seeks(&self) -> Vec<f32> {
        self.seeks.borrow().clone()
    }

    fn previews(&self) -> Vec<f32> {
        self.previews.borrow().clone()
    }
}

/// クリックした位置の比率でシークする（`mock_single_click` は要素の中央＝比率 0.5 を押す）。
#[test]
fn click_seeks_to_the_clicked_ratio() {
    let probe = Probe::new(true);
    probe.bar().mock_single_click(PointerEventButton::Left);

    let seeks = probe.seeks();
    assert_eq!(seeks.len(), 1, "a click must seek exactly once: {seeks:?}");
    assert!(
        (seeks[0] - 0.5).abs() < RATIO_TOLERANCE,
        "a click at the center must seek to the middle, got {}",
        seeks[0]
    );
    assert!(
        !probe.window.get_scrubbing(),
        "scrubbing must be cleared after the pointer is released"
    );
}

/// ドラッグ中はプレビューだけが追従し、離した位置で 1 回だけシークする。
#[test]
fn drag_previews_and_seeks_only_on_release() {
    let probe = Probe::new(true);
    // 中央（比率 0.5）から右へ、バー幅の 90% の位置まで引く。
    probe
        .bar()
        .mock_drag(probe.point_at(0.9), PointerEventButton::Left);

    let previews = probe.previews();
    assert!(
        previews.len() > 2,
        "dragging must preview repeatedly: {previews:?}"
    );
    assert!(
        previews.windows(2).all(|pair| pair[0] <= pair[1]),
        "previews must follow the pointer to the right: {previews:?}"
    );
    assert!(
        probe.scrubbing_at_preview.borrow().iter().all(|f| *f),
        "scrubbing must be true while previewing, so the playback tick stops overwriting"
    );

    let seeks = probe.seeks();
    assert_eq!(
        seeks.len(),
        1,
        "the audio must move once, on release: {seeks:?}"
    );
    assert!(
        (seeks[0] - 0.9).abs() < RATIO_TOLERANCE,
        "the seek must use the release position, got {}",
        seeks[0]
    );
    assert!(
        !probe.window.get_scrubbing(),
        "scrubbing must be cleared after the pointer is released"
    );
}

/// バーの外へドラッグして離しても、比率は 0.0〜1.0 に収まる。
#[test]
fn dragging_outside_the_bar_clamps_the_ratio() {
    let probe = Probe::new(true);
    let outside_left =
        LogicalPosition::new(probe.point_at(0.0).x - WINDOW_WIDTH, probe.point_at(0.0).y);
    probe
        .bar()
        .mock_drag(outside_left, PointerEventButton::Left);

    let seeks = probe.seeks();
    assert_eq!(seeks.len(), 1, "the release must seek once: {seeks:?}");
    assert_eq!(
        seeks[0], 0.0,
        "dragging left of the bar must clamp to the start"
    );
    assert!(
        probe.previews().iter().all(|r| (0.0..=1.0).contains(r)),
        "previews must stay within the bar: {:?}",
        probe.previews()
    );
}

/// 左ボタン以外では何も起きない（右クリックでシークさせない）。
#[test]
fn right_button_neither_previews_nor_seeks() {
    let probe = Probe::new(true);
    probe.bar().mock_single_click(PointerEventButton::Right);
    probe
        .bar()
        .mock_drag(probe.point_at(0.2), PointerEventButton::Right);

    assert!(
        probe.previews().is_empty(),
        "a right-button drag must not preview: {:?}",
        probe.previews()
    );
    assert!(
        probe.seeks().is_empty(),
        "a right-button drag must not seek: {:?}",
        probe.seeks()
    );
    assert!(!probe.window.get_scrubbing());
}

/// `seekable` が false（再生不可・全体長不明のセッション）では表示専用に縮退する。
#[test]
fn not_seekable_ignores_click_and_drag() {
    let probe = Probe::new(false);
    probe.bar().mock_single_click(PointerEventButton::Left);
    probe
        .bar()
        .mock_drag(probe.point_at(0.8), PointerEventButton::Left);

    assert!(
        probe.previews().is_empty(),
        "a display-only bar must not preview: {:?}",
        probe.previews()
    );
    assert!(
        probe.seeks().is_empty(),
        "a display-only bar must not seek: {:?}",
        probe.seeks()
    );
    assert!(
        !probe.window.get_scrubbing(),
        "a display-only bar must never enter scrubbing"
    );
}
