use lianli_shared::device_id::DeviceFamily;
use lianli_shared::screen::{screen_info_for, ScreenInfo};

#[test]
fn h264_screens_have_sufficient_payload() {
    let families = [
        DeviceFamily::HydroShiftLcd,
        DeviceFamily::Galahad2Lcd,
        DeviceFamily::HydroShift2Lcd,
        DeviceFamily::HydroShift2OledCurveLcd,
        DeviceFamily::UniversalScreen,
        DeviceFamily::TlFlexLcd,
        DeviceFamily::SlInfFlexLcd,
    ];
    for family in families {
        if let Some(screen) = screen_info_for(family) {
            let worst_case = (screen.width * screen.height * 3) as usize;
            assert!(
                screen.max_payload >= worst_case,
                "{family:?}: max_payload {} < worst-case JPEG {} ({}x{})",
                screen.max_payload,
                worst_case,
                screen.width,
                screen.height
            );
        }
    }
}

#[test]
fn hs2_oled_curve_has_correct_flags() {
    let screen = ScreenInfo::HYDROSHIFT2_OLED_CURVE;
    assert!(screen.png);
    assert_eq!(screen.play_count, 1);
    assert!(screen.h264);
}

#[test]
fn disabled_h264_screens_are_false() {
    assert!(!ScreenInfo::LANCOOL_207.h264);
    assert!(!ScreenInfo::VISION_9P2.h264);
}

#[test]
fn only_hs2_oled_has_nonzero_play_count() {
    let screens = [
        ScreenInfo::WIRELESS_LCD,
        ScreenInfo::TLLCD,
        ScreenInfo::AIO_LCD_480,
        ScreenInfo::HYDROSHIFT2,
        ScreenInfo::LANCOOL_207,
        ScreenInfo::UNIVERSAL_SCREEN,
        ScreenInfo::VISION_9P2,
        ScreenInfo::FLEX_LCD,
    ];
    for screen in &screens {
        assert_eq!(screen.play_count, 0);
    }
}

#[test]
fn all_screen_dimensions_even() {
    let screens = [
        ScreenInfo::WIRELESS_LCD,
        ScreenInfo::TLLCD,
        ScreenInfo::AIO_LCD_480,
        ScreenInfo::HYDROSHIFT2,
        ScreenInfo::HYDROSHIFT2_OLED_CURVE,
        ScreenInfo::LANCOOL_207,
        ScreenInfo::UNIVERSAL_SCREEN,
        ScreenInfo::VISION_9P2,
        ScreenInfo::FLEX_LCD,
    ];
    for screen in &screens {
        assert_eq!(screen.width % 2, 0, "width must be even: {}", screen.width);
        assert_eq!(
            screen.height % 2,
            0,
            "height must be even: {}",
            screen.height
        );
    }
}

#[test]
fn screen_info_for_returns_correct_resolution() {
    let hs2 = screen_info_for(DeviceFamily::HydroShift2OledCurveLcd).unwrap();
    assert_eq!((hs2.width, hs2.height), (1080, 2288));

    let universal = screen_info_for(DeviceFamily::UniversalScreen).unwrap();
    assert_eq!((universal.width, universal.height), (480, 1920));

    let lancool = screen_info_for(DeviceFamily::Lancool207).unwrap();
    assert_eq!((lancool.width, lancool.height), (720, 1472));
}
