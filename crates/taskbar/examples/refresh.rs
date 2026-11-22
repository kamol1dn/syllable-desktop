//! Diagnostic: what DWM reports as the composition rate.
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::Performance::*;

fn main() {
    unsafe {
        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let mut info = DWM_TIMING_INFO { cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
        let r = DwmGetCompositionTimingInfo(HWND::default(), &mut info);
        let rate = info.rateRefresh;
        let (num, den) = (rate.uiNumerator, rate.uiDenominator);
        println!(
            "{r:?} freq={freq} qpcRefreshPeriod={} rateRefresh={}/{} -> {:.2} Hz",
            { info.qpcRefreshPeriod },
            num,
            den,
            freq as f64 / { info.qpcRefreshPeriod } as f64
        );
    }
}
