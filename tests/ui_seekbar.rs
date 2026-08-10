//! シークバー（`ui/recordings-window.slint` の `SeekBar`）の操作を、実際のポインタイベントで
//! 検証する。ビルド・clippy・純粋関数の単体テストでは「TouchArea の配線」を検証できないため、
//! Slint のテストバックエンドでクリック・ドラッグを流して振る舞いを固定する。
//!
//! 見た目（レイアウト・配色）の確認は `examples/transcript_view.rs` ＋ screencapture
//! （`docs/rules/slint.md`）。こちらは「操作したら何が起きるか」を担当する。
//!
//! 要素を探す `ElementHandle` は生成コードのデバッグ情報を要求する。有無で各テストを
//! ignore へ切り替えるので、無いプロファイルでは理由つきで「無視」と表示される
//! （切り替えの条件と有効化方法は `docs/rules/slint.md`）。

slint::include_modules!();

mod ui_support;

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;
use slint::LogicalPosition;
use slint::platform::{PointerEventButton, WindowEvent};

/// テスト用のウィンドウサイズ（`src/main.rs` の RECORDINGS_WIDTH/HEIGHT と同じ）。要素の座標は
/// レイアウト結果から解決するので、この値自体はアサートに使わない。
const WINDOW_WIDTH: f32 = 720.0;
const WINDOW_HEIGHT: f32 = 540.0;

/// 比率の許容誤差。`mock_drag` は最後に必ず目標座標へ移動するので、ずれは f32 の丸め程度しか
/// 出ない。バー幅 400px 前後に対して 1e-3（≒0.4px）まで絞り、座標→比率の対応が変わったら
/// 落ちるようにする。
const RATIO_TOLERANCE: f32 = 1e-3;

/// ドラッグ中に届いた 1 回のプレビュー。比率と、その時点で `scrubbing` が立っていたか。
/// （2 本の Vec に分けると「同じ添字が同じイベント」という前提が型に出ない）
#[derive(Debug, Clone, Copy)]
struct Preview {
    ratio: f32,
    scrubbing: bool,
}

/// Recordings ウィンドウと、シークバーのコールバックの記録。
struct Probe {
    window: RecordingsWindow,
    /// `scrub-preview`（ドラッグ中のプレビュー通知）の記録。
    previews: Rc<RefCell<Vec<Preview>>>,
    /// `seek-to-ratio`（クリック確定・ドラッグ終了）で渡された比率。
    seeks: Rc<RefCell<Vec<f32>>>,
}

impl Probe {
    /// 操作できる状態（再生可能・全体長が分かる）のシークバー。
    fn seekable() -> Self {
        Self::new(true)
    }

    /// 表示専用に縮退した状態（再生不可・全体長不明のセッション相当）のシークバー。
    fn display_only() -> Self {
        Self::new(false)
    }

    fn new(seekable: bool) -> Self {
        ui_support::init_backend();
        let window = RecordingsWindow::new().expect("creating the window should succeed");
        window
            .window()
            .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
        // シークバーは詳細ペイン（`has-selection` の内側）にあるので、選択済みの状態にする。
        window.set_has_selection(true);
        window.set_playable(true);
        window.set_seekable(seekable);

        let previews: Rc<RefCell<Vec<Preview>>> = Rc::new(RefCell::new(Vec::new()));
        let seeks: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let previews = Rc::clone(&previews);
            let weak = window.as_weak();
            window.on_scrub_preview(move |ratio| {
                let scrubbing = weak
                    .upgrade()
                    .expect("the window outlives the callback in this test")
                    .get_scrubbing();
                previews.borrow_mut().push(Preview { ratio, scrubbing });
            });
        }
        {
            let seeks = Rc::clone(&seeks);
            window.on_seek_to_ratio(move |ratio| seeks.borrow_mut().push(ratio));
        }

        let probe = Self {
            window,
            previews,
            seeks,
        };
        // 以降のアサートは「バー上の比率」に依存するので、レイアウトが解決できていることを先に
        // 確かめる（幅 0 のまま進むと、無関係なメッセージで落ちて原因が分かりにくい）。
        assert!(
            probe.bar().size().width > 0.0,
            "the seek bar must have a resolved width before simulating pointer events"
        );
        probe
    }

    /// シークバー本体。座標はレイアウト結果から取るので、周辺の構成が変わっても追従する。
    fn bar(&self) -> ElementHandle {
        ElementHandle::find_by_element_type_name(&self.window, "SeekBar")
            .next()
            .expect(
                "no SeekBar was found: either the Playback section changed, \
                 or the generated code has no debug info (see build.rs)",
            )
    }

    /// バー上の比率に対応する絶対座標（縦はバーの中央）。比率 0.0〜1.0 の外も指定できる
    /// （バーの外へドラッグする検証に使う）。
    fn point_at(&self, ratio: f32) -> LogicalPosition {
        let bar = self.bar();
        let position = bar.absolute_position();
        let size = bar.size();
        LogicalPosition::new(
            position.x + size.width * ratio,
            position.y + size.height / 2.0,
        )
    }

    /// 指定位置で押す（`mock_single_click` は要素の中央しか押せないため、位置を選ぶときに使う）。
    fn press_at(&self, ratio: f32) {
        let position = self.point_at(ratio);
        let window = self.window.window();
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    }

    /// 指定位置で離す。`PointerMoved` は送らない（同座標でも `moved` が発火して比率が
    /// 再設定されるため、押下位置が使われているかを検証できなくなる）。
    fn release_at(&self, ratio: f32) {
        let window = self.window.window();
        window.dispatch_event(WindowEvent::PointerReleased {
            position: self.point_at(ratio),
            button: PointerEventButton::Left,
        });
    }

    fn previews(&self) -> Vec<Preview> {
        self.previews.borrow().clone()
    }

    fn seeks(&self) -> Vec<f32> {
        self.seeks.borrow().clone()
    }
}

/// 押した位置の比率でシークする（中央ではない位置で、ポインタ座標が使われていることを見る）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn click_seeks_to_the_pressed_ratio() {
    let probe = Probe::seekable();
    probe.press_at(0.25);
    // 押した時点でプレビューが押下位置で来る（`down` 分岐が押下座標を使っている）。
    let pressed = probe.previews();
    assert_eq!(pressed.len(), 1, "pressing must preview once: {pressed:?}");
    assert!(
        (pressed[0].ratio - 0.25).abs() < RATIO_TOLERANCE,
        "the press must preview the pressed position, got {}",
        pressed[0].ratio
    );
    probe.release_at(0.25);

    let seeks = probe.seeks();
    assert_eq!(seeks.len(), 1, "a click must seek exactly once: {seeks:?}");
    assert!(
        (seeks[0] - 0.25).abs() < RATIO_TOLERANCE,
        "a click at a quarter of the bar must seek there, got {}",
        seeks[0]
    );
    assert!(
        !probe.window.get_scrubbing(),
        "scrubbing must be cleared after the pointer is released"
    );
}

/// ドラッグ中はプレビューだけが追従し、離した位置で 1 回だけシークする。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn drag_previews_and_seeks_only_on_release() {
    let probe = Probe::seekable();
    // `mock_drag` は要素の中央（比率 0.5）から始まる。右へバー幅の 90% の位置まで引く。
    probe
        .bar()
        .mock_drag(probe.point_at(0.9), PointerEventButton::Left);

    let previews = probe.previews();
    assert!(
        previews.len() > 2,
        "dragging must preview repeatedly: {previews:?}"
    );
    assert!(
        previews
            .windows(2)
            .all(|pair| pair[0].ratio <= pair[1].ratio),
        "previews must follow the pointer to the right: {previews:?}"
    );
    assert!(
        previews.iter().all(|preview| preview.scrubbing),
        "scrubbing must be true while previewing, so the playback tick stops overwriting: \
         {previews:?}"
    );
    let last = previews
        .last()
        .expect("the drag emitted previews, so there is a last one");
    assert!(
        (last.ratio - 0.9).abs() < RATIO_TOLERANCE,
        "the last preview must reach the release position, got {}",
        last.ratio
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
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn dragging_outside_the_bar_clamps_the_ratio() {
    let probe = Probe::seekable();
    // 比率 -1.0 ＝ バー 1 本ぶん左（バーの外）。
    probe
        .bar()
        .mock_drag(probe.point_at(-1.0), PointerEventButton::Left);

    let seeks = probe.seeks();
    assert_eq!(seeks.len(), 1, "the release must seek once: {seeks:?}");
    assert_eq!(
        seeks[0], 0.0,
        "dragging left of the bar must clamp to the start"
    );
    let previews = probe.previews();
    assert!(
        previews
            .iter()
            .all(|preview| (0.0..=1.0).contains(&preview.ratio)),
        "previews must stay within the bar: {previews:?}"
    );
}

/// 取り消し（押したままウィンドウの外へ出る）はシークせず、`scrubbing` を畳む。畳み忘れると
/// 再生 tick が表示を更新できないまま固まる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn leaving_the_window_cancels_the_scrub_without_seeking() {
    let probe = Probe::seekable();
    probe.press_at(0.5);
    assert!(
        probe.window.get_scrubbing(),
        "pressing the bar must start scrubbing"
    );

    probe
        .window
        .window()
        .dispatch_event(WindowEvent::PointerExited);

    assert!(
        !probe.window.get_scrubbing(),
        "leaving the window must collapse scrubbing so the playback tick resumes"
    );
    assert!(
        probe.seeks().is_empty(),
        "a cancelled scrub must not seek: {:?}",
        probe.seeks()
    );
}

/// 左ボタン以外では何も起きない（右クリックでシークさせない）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn right_button_neither_previews_nor_seeks() {
    let probe = Probe::seekable();
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
    assert!(
        !probe.window.get_scrubbing(),
        "a right-button press must never enter scrubbing"
    );
}

/// `seekable` が false（再生不可・全体長不明のセッション）では表示専用に縮退する。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn not_seekable_ignores_click_and_drag() {
    let probe = Probe::display_only();
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

/// 掴んでいる間は、キーボードのシークが効かない。
///
/// 掴むとフォーカスもこの部品へ移る（そのまま矢印キーで微調整できるように）ので、キーは届く。
/// そこで確定させてしまうと、**音だけ飛んで表示は掴んだ位置のまま**になり、離した瞬間に
/// 掴んだ位置へ戻る（再生 tick は `scrubbing` 中 `progress` を上書きしないので、表示は
/// 動かない）。確定はポインタを離したときの 1 回に集める。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn keys_do_not_seek_while_the_bar_is_grabbed() {
    let probe = Probe::seekable();
    probe.press_at(0.3);
    assert!(
        probe.window.get_scrubbing(),
        "pressing must start a scrub, otherwise this test checks nothing"
    );

    probe
        .window
        .window()
        .dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::RightArrow.into(),
        });
    assert!(
        probe.seeks().is_empty(),
        "a key press during a scrub must not move the audio: {:?}",
        probe.seeks()
    );

    // 離したぶんだけがシークになる（キーの押下が余分に混ざっていない）。
    probe.release_at(0.3);
    assert_eq!(
        probe.seeks().len(),
        1,
        "only the release must seek: {:?}",
        probe.seeks()
    );
}

/// 掴んでいなければ、左右キーでシークできる（上のテストが「キーが常に死んでいる」ことを
/// 通してしまわないように、生きている側も固定する）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn keys_seek_once_the_bar_is_released() {
    let probe = Probe::seekable();
    probe.press_at(0.3);
    probe.release_at(0.3);
    let after_release = probe.seeks().len();

    probe
        .window
        .window()
        .dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::RightArrow.into(),
        });
    assert_eq!(
        probe.seeks().len(),
        after_release + 1,
        "the right arrow must seek once the pointer is released: {:?}",
        probe.seeks()
    );
}
