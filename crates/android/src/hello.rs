//! M0's smoke test (spec §12): one page in the theme's colours and a
//! button that counts taps. If this shows up on the emulator, the NDK,
//! the Skia prebuilt, the Java helper build, Gradle and `adb install`
//! all work — before the whole engine is in the picture.
//!
//! Built with `--no-default-features --features hello`; the colours are
//! `theme.slint`'s, copied rather than imported so this file depends on
//! nothing but `slint`.

slint::slint! {
    export component Hello inherits Window {
        in-out property <int> taps: 0;
        background: #1A1D21;

        VerticalLayout {
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
                text: "Android M0 — toolchain hello";
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
