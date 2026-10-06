//! M0's smoke test (IOS-SPEC.md §12): one page in the theme's colours
//! and a button that counts taps. If this shows up in the simulator and
//! on the phone, Xcode's cargo phase, the Rust iOS targets, the Skia
//! prebuilt, Metal, signing and installation all work — before the
//! whole engine is in the picture.
//!
//! Built with `--no-default-features --features hello` (`ios/run.sh
//! --hello`); the colours are `theme.slint`'s, copied rather than
//! imported so this file depends on nothing but `slint`. The page keeps
//! clear of the notch and the home indicator with the window's
//! `safe-area-insets`, which is the first thing worth seeing work.

slint::slint! {
    export component Hello inherits Window {
        in-out property <int> taps: 0;
        background: #1A1D21;

        VerticalLayout {
            x: root.safe-area-insets.left;
            y: root.safe-area-insets.top;
            width: root.width - root.safe-area-insets.left - root.safe-area-insets.right;
            height: root.height - root.safe-area-insets.top - root.safe-area-insets.bottom;
            padding: 32px;
            spacing: 24px;
            alignment: center;

            Text {
                text: "baucord";
                color: #D1D2D3;
                font-size: 40px;
                font-weight: 700;
                horizontal-alignment: center;
            }
            Text {
                text: "iOS M0 — toolchain hello";
                color: #929598;
                font-size: 18px;
                horizontal-alignment: center;
            }
            Rectangle {
                height: 56px;
                border-radius: 10px;
                background: touch.pressed ? #0E5082 : #1264A3;
                touch := TouchArea {
                    clicked => { root.taps += 1; }
                }
                Text {
                    text: root.taps == 0 ? "Tap me" : "Tapped " + root.taps + "×";
                    color: #FFFFFF;
                    font-size: 18px;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
        }
    }
}

pub fn run() {
    let hello = Hello::new().expect("create the hello window");
    hello.run().expect("run the hello window");
}
