#![no_std]
#![no_main]

mod clocks;

mod gameboy;
mod hardware;

mod rp_hal;
mod ui;
mod util;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use core::cell::RefCell;

use display_interface::WriteOnlyDataCommand;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::prelude::{DrawTarget, Point};

use embedded_hal::digital::OutputPin;
use ui::rom_select::select_rom;

use embedded_sdmmc::sdcard::AcquireOpts;
use gb_core::hardware::boot_rom::Bootrom;
use gb_core::hardware::cartridge::Cartridge;
use mipidsi::models::Model;
use mipidsi::options::{Orientation, Rotation};
use mipidsi::Display;
use panic_probe as _;
use ui::loading::LoadingScreen;
extern crate alloc;

use embedded_sdmmc::{SdCard, VolumeManager};
use gameboy::display::GameboyLineBufferDisplay;
use gameboy::{GameEmulationHandler, GameboyButtonHandler, InputButtonMapper};
use gb_core::gameboy::GameBoy;
use hal::fugit::RateExtU32;

use hardware::display::ScreenScaler;

use rp235x_hal::timer::TimerDevice;
use rp235x_hal::uart::{DataBits, StopBits, UartConfig};
use rp235x_hal::{spi, Clock};
use rp_hal::hal::dma::DMAExt;
use rp_hal::hal::pio::PIOExt;

// Alias for our HAL crate
use rp_hal::hal;
// Some things we need
use embedded_alloc::LlffHeap as Heap;

//Include selected display driver
include!(concat!(env!("OUT_DIR"), "/generated_display_driver.rs"));

/// Tell the Boot ROM about our application
#[link_section = ".start_block"]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

/// External high-speed crystal on the Raspberry Pi Pico 2 board is 12 MHz.
/// Adjust if your board has a different frequency
const XTAL_FREQ_HZ: u32 = 12_000_000u32;

#[global_allocator]
static ALLOCATOR: Heap = Heap::empty();

static SERIAL: static_cell::StaticCell<
    rp235x_hal::uart::UartPeripheral<
        rp235x_hal::uart::Enabled,
        rp235x_hal::pac::UART0,
        (
            rp235x_hal::gpio::Pin<
                rp235x_hal::gpio::bank0::Gpio0,
                rp235x_hal::gpio::FunctionUart,
                rp235x_hal::gpio::PullDown,
            >,
            rp235x_hal::gpio::Pin<
                rp235x_hal::gpio::bank0::Gpio1,
                rp235x_hal::gpio::FunctionUart,
                rp235x_hal::gpio::PullDown,
            >,
        ),
    >,
> = static_cell::StaticCell::new();

#[const_env::from_env("DISPLAY_WIDTH")]
const DISPLAY_WIDTH: u16 = 240;
#[const_env::from_env("DISPLAY_HEIGHT")]
const DISPLAY_HEIGHT: u16 = 320;
#[const_env::from_env]
const GAMEBOY_RENDER_WIDTH: u16 = 320;
#[const_env::from_env]
const GAMEBOY_RENDER_HEIGHT: u16 = 240;
#[const_env::from_env]
const DISPLAY_ROTATION: u16 = 0;
#[const_env::from_env]
const DISPLAY_MIRRORED: bool = false;
#[const_env::from_env]
const DISPLAY_COLOR_INVERT: bool = false;

const RENDER_WIDTH: u16 = if DISPLAY_ROTATION == 90 || DISPLAY_ROTATION == 270 {
    DISPLAY_HEIGHT
} else {
    DISPLAY_WIDTH
};
const RENDER_HEIGHT: u16 = if DISPLAY_ROTATION == 90 || DISPLAY_ROTATION == 270 {
    DISPLAY_WIDTH
} else {
    DISPLAY_HEIGHT
};

#[hal::entry]
fn main() -> ! {
    const {
        assert!(
            RENDER_WIDTH >= GAMEBOY_RENDER_WIDTH,
            "Gameboy render width cannot be smaller than the width of the screen"
        )
    }
    const {
        assert!(
            RENDER_HEIGHT >= GAMEBOY_RENDER_HEIGHT,
            "Gameboy render height cannot be smaller than the width of the screen"
        )
    }
    let mut pac = hal::pac::Peripherals::take().unwrap();

    // Grab our singleton objects
    let sio = hal::Sio::new(pac.SIO);
    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // Set up the watchdog driver - needed by the clock setup code
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);

    pac.POWMAN.vreg_ctrl().write(|w| unsafe {
        w.bits(0x5AFE_0000);
        w.unlock().set_bit();
        w.ht_th().bits(0b101);
        w.rst_n().set_bit();
        w
    });
    while pac.POWMAN.vreg().read().update_in_progress().bit_is_set() {
        rp235x_hal::arch::nop();
    }
    pac.POWMAN
        .vreg()
        .modify(|_, w| unsafe { w.bits(0x5AFE_0000).vsel().bits(0b01111) }); // 0b01111 = 1.30V

    while pac.POWMAN.vreg().read().update_in_progress().bit_is_set() {
        rp235x_hal::arch::nop();
    }

    let clocks = clocks::configure_overclock(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let mut timer: rp235x_hal::Timer<rp235x_hal::timer::CopyableTimer0> =
        hal::Timer::new_timer0(pac.TIMER0, &mut pac.RESETS, &clocks);

    ///////////////////UART!
    let uart0_pins = (
        pin_select!(pins, env!("PIN_UART_RX")).into_function(),
        pin_select!(pins, env!("PIN_UART_TX")).into_function(),
    );

    let uart0 = hal::uart::UartPeripheral::new(pac.UART0, uart0_pins, &mut pac.RESETS)
        .enable(
            UartConfig::new(115200.Hz(), DataBits::Eight, None, StopBits::One),
            clocks.peripheral_clock.freq(),
        )
        .unwrap();
    defmt_serial::defmt_serial(SERIAL.init(uart0));

    defmt::info!("Console Start");

    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 300_000 + (0x4000 * 6);
        static mut HEAP: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { ALLOCATOR.init(HEAP.as_ptr() as usize, HEAP_SIZE) }
    }

    let mut led_pin = pins.gpio25.into_push_pull_output();

    let (mut pio_0, sm0_0, sm0_1, _, _) = pac.PIO0.split(&mut pac.RESETS);

    let (mut pio_1, sm_1_0, _, _, _) = pac.PIO1.split(&mut pac.RESETS);
    let dma = pac.DMA.split(&mut pac.RESETS);

    ///////////////////////////////SD CARD
    let spi_sclk: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_select!(pins, env!("PIN_SD_CARD_SCLK")).into_function::<hal::gpio::FunctionSpi>();

    let spi_mosi: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_select!(pins, env!("PIN_SD_CARD_MOSI")).into_function::<hal::gpio::FunctionSpi>();
    let spi_cs = pin_select!(pins, env!("PIN_SD_CARD_CS")).into_push_pull_output();
    let spi_miso: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_select!(pins, env!("PIN_SD_CARD_MISO")).into_function::<hal::gpio::FunctionSpi>();

    // Create the SPI driver instance for the SPI0 device
    let spi = spi::Spi::<_, _, _, 8>::new(pac.SPI1, (spi_mosi, spi_miso, spi_sclk));
    let spi = spi.init(
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
        200.kHz(),
        embedded_hal::spi::MODE_0,
    );

    let exclusive_spi = embedded_hal_bus::spi::ExclusiveDevice::new_no_delay(spi, spi_cs).unwrap();
    let sdcard = SdCard::new_with_options(
        exclusive_spi,
        timer,
        AcquireOpts {
            acquire_retries: 100,
            use_crc: true,
        },
    );

    let mut volume_mgr = VolumeManager::new(sdcard, hardware::sdcard::DummyTimesource::default());
    let mut volume0 = volume_mgr
        .open_volume(embedded_sdmmc::VolumeIdx(0))
        .unwrap();

    let rom_list = Rc::new(RefCell::new(alloc::vec::Vec::<String>::new()));
    let mut root_dir = volume0.open_root_dir().unwrap();
    root_dir
        .iterate_dir(|dir_entry| {
            let extension = String::from_utf8(dir_entry.name.extension().to_vec()).unwrap();
            if extension.eq("GB") {
                let rom_name =
                    String::from_utf8(dir_entry.name.base_name().to_vec()).unwrap() + ".GB";
                rom_list.borrow_mut().push(rom_name);
            }
        })
        .unwrap();
    let rom_list = rom_list.take();
    root_dir.close().unwrap();
    volume0.close().unwrap();
    let boot_rom = load_boot_rom(&mut volume_mgr);

    //////////////////////AUDIO SETUP

    let sample_rate: u32 = 16_000;
    let clock_divider: u32 = clocks.system_clock.freq().to_Hz() * 4 / sample_rate;

    let int_divider = (clock_divider >> 8) as u16;
    let frak_divider = (clock_divider & 0xFF) as u8;

    let i2s_din = pin_select!(pins, env!("PIN_I2S_DIN")).into_function::<hal::gpio::FunctionPio1>();
    let i2s_bclk =
        pin_select!(pins, env!("PIN_I2S_BCLK")).into_function::<hal::gpio::FunctionPio1>();
    let i2s_lrc = pin_select!(pins, env!("PIN_I2S_LRC")).into_function::<hal::gpio::FunctionPio1>();
    let audio_buffer: &'static mut [u16] =
        cortex_m::singleton!(: [u16; (2000 * 3) * 3]  = [0u16;  (2000 * 3) * 3 ])
            .unwrap()
            .as_mut_slice();
    let i2s_interface = hardware::sound::I2sPioInterface::new(
        sample_rate,
        dma.ch2,
        dma.ch3,
        (int_divider as u16, frak_divider as u8),
        &mut pio_1,
        sm_1_0,
        (i2s_bclk.id().num, i2s_lrc.id().num),
        i2s_din.id().num,
        audio_buffer,
    );

    let screen = GameboyLineBufferDisplay::new(timer);

    let display_buffer: &'static mut [u16] =
    cortex_m::singleton!(: [u16;(GAMEBOY_RENDER_WIDTH as usize) * 3]  = [0u16; (GAMEBOY_RENDER_WIDTH as usize ) * 3 ])
        .unwrap()
        .as_mut_slice();
    let mut screen_data_cs = pin_select!(pins, env!("PIN_SCREEN_CS")).into_push_pull_output();
    screen_data_cs.set_low().unwrap();

    let screen_data_command_pin = pin_select!(pins, env!("PIN_SCREEN_DC")).into_push_pull_output();
    let display_reset = pin_select!(pins, env!("PIN_SCREEN_RESET")).into_push_pull_output();
    let spi_sclk =
        pin_select!(pins, env!("PIN_SCREEN_SCLK")).into_function::<hal::gpio::FunctionPio0>();
    let spi_mosi =
        pin_select!(pins, env!("PIN_SCREEN_MOSI")).into_function::<hal::gpio::FunctionPio0>();

    let streamer = hardware::display::DmaStreamer::new(dma.ch0, dma.ch1, display_buffer);
    let display_interface = hardware::display::SpiPioDmaInterface::new(
        (3, 0),
        screen_data_command_pin,
        &mut pio_0,
        sm0_1,
        sm0_0,
        spi_sclk.id().num,
        spi_mosi.id().num,
        streamer,
        timer,
    );

    let display_builder = mipidsi::Builder::new(DisplayDriver, display_interface)
        .reset_pin(display_reset)
        .display_size(DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16)
        .color_order(mipidsi::options::ColorOrder::Bgr)
        .invert_colors(if DISPLAY_COLOR_INVERT {
            mipidsi::options::ColorInversion::Inverted
        } else {
            mipidsi::options::ColorInversion::Normal
        })
        .orientation(Orientation {
            rotation: match DISPLAY_ROTATION {
                0 => Rotation::Deg0,
                90 => Rotation::Deg90,
                180 => Rotation::Deg180,
                270 => Rotation::Deg270,
                _ => unreachable!(),
            },
            mirrored: DISPLAY_MIRRORED,
        });

    let mut display = display_builder.init(&mut timer).unwrap();

    ////////////////////// JOYPAD
    let mut b_button = pin_select!(pins, env!("PIN_B_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut a_button = pin_select!(pins, env!("PIN_A_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut right_button = pin_select!(pins, env!("PIN_RIGHT_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut down_button = pin_select!(pins, env!("PIN_DOWN_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut left_button = pin_select!(pins, env!("PIN_LEFT_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut up_button = pin_select!(pins, env!("PIN_UP_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut select_button = pin_select!(pins, env!("PIN_SELECT_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();
    let mut start_button = pin_select!(pins, env!("PIN_START_BUTTON"))
        .into_pull_up_input()
        .into_dyn_pin();

    let selected_rom = select_rom(
        &mut display,
        rom_list.as_slice(),
        timer,
        &mut up_button,
        &mut down_button,
        &mut a_button,
    )
    .unwrap();

    let name = rom_list[selected_rom].clone();
    defmt::info!("Menu END: {}", defmt::Display2Format(&name));

    #[cfg(feature = "psram_rom")]
    let cartridge = {
        defmt::info!("Using PRSAM");

        let _ =
            pin_select!(pins, env!("PIN_PSRAM_CS")).into_function::<hal::gpio::FunctionXipCs1>();
        let psram_size = hardware::psram::psram_init(
            clocks.peripheral_clock.freq().to_Hz(),
            &pac.QMI,
            &pac.XIP_CTRL,
        );

        let psram = unsafe {
            const PSRAM_ADDRESS: usize = 0x11000000;
            let ptr = PSRAM_ADDRESS as *mut u8; // Using u8 for byte array
            let slice: &'static mut [u8] =
                alloc::slice::from_raw_parts_mut(ptr, psram_size as usize);
            slice
        };

        let cartridge = load_rom_to_psram(&mut display, volume_mgr, timer, &name, psram, |db| {
            db.mark_card_uninit();
        });
        cartridge
    };
    #[cfg(not(feature = "psram_rom"))]
    let cartridge = load_rom(&mut display, volume_mgr, &name, timer, |bd| {
        bd.mark_card_uninit();
    });

    let gameboy = GameBoy::create(screen, cartridge, boot_rom, Box::new(i2s_interface));

    let button_handler = InputButtonMapper::new(
        &mut a_button,
        &mut b_button,
        &mut start_button,
        &mut select_button,
        &mut up_button,
        &mut down_button,
        &mut left_button,
        &mut right_button,
    );
    led_pin.set_high().unwrap();

    display.clear(Rgb565::BLACK).unwrap();
    run_game_boy(gameboy, display, button_handler, timer);
    loop {
        crate::hal::arch::nop();
    }
}

#[inline(never)]
pub fn run_game_boy<'a, D: TimerDevice, DI, M, RST, BH: GameboyButtonHandler<'a>>(
    mut gameboy: GameBoy<'a, GameboyLineBufferDisplay<D>>,
    mut display: Display<DI, M, RST>,
    mut button_handler: BH,
    timer: crate::hal::Timer<D>,
) where
    DI: WriteOnlyDataCommand,
    M: Model<ColorFormat = Rgb565>,
    RST: OutputPin,
{
    const MIDDLE_HEIGHT: u16 = (RENDER_HEIGHT - GAMEBOY_RENDER_HEIGHT) / 2;
    const MIDDLE_WIDTH: u16 = (RENDER_WIDTH - GAMEBOY_RENDER_WIDTH) / 2;
    let scaler: ScreenScaler<
        { 144 - 1 },
        160,
        { GAMEBOY_RENDER_HEIGHT as usize },
        { GAMEBOY_RENDER_WIDTH as usize },
    > = ScreenScaler::new();
    let mut loop_counter: usize = 0;
    loop {
        let start_time = timer.get_counter();
        display
            .set_pixels(
                MIDDLE_WIDTH,
                MIDDLE_HEIGHT,
                (GAMEBOY_RENDER_WIDTH - 1) as u16 + MIDDLE_WIDTH,
                (GAMEBOY_RENDER_HEIGHT - 1) as u16 + MIDDLE_HEIGHT,
                scaler.scale_iterator(GameEmulationHandler::new(&mut gameboy, &mut button_handler)),
            )
            .unwrap();

        let end_time: hal::fugit::Instant<u64, 1, 1000000> = timer.get_counter();
        let diff: fugit::Duration<u64, 1, 1000000> = end_time - start_time;
        let milliseconds = diff.to_millis();
        defmt::info!(
            "Loop: {}, Time elapsed: {}:{}",
            loop_counter,
            milliseconds / 1000,
            milliseconds % 1000
        );
        loop_counter += 1;
    }
}

/// Program metadata for `picotool info`
#[link_section = ".bi_entries"]
#[used]
pub static PICOTOOL_ENTRIES: [hal::binary_info::EntryAddr; 5] = [
    hal::binary_info::rp_cargo_bin_name!(),
    hal::binary_info::rp_cargo_version!(),
    hal::binary_info::rp_program_description!(c"gb-rp2350"),
    hal::binary_info::rp_cargo_homepage_url!(),
    hal::binary_info::rp_program_build_attribute!(),
];

#[cfg(feature = "flash_rom")]
const ROM_FLASH_SIZE: usize = 1024 * 1024;

#[cfg(feature = "flash_rom")]
#[link_section = ".rodata"]
static FLASH_ROM_DATA: hardware::flash::FlashBlock<ROM_FLASH_SIZE> = hardware::flash::FlashBlock {
    data: core::cell::UnsafeCell::new([0x55u8; ROM_FLASH_SIZE]),
};

#[inline(never)]
#[cfg(feature = "flash_rom")]
fn load_rom<
    'a,
    DISPLAY: DrawTarget<Color = Rgb565>,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    DR: Fn(&mut D) + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    display: &mut DISPLAY,
    mut volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    rom_name: &str,
    timer: crate::hal::Timer<DT>,
    device_reset: DR,
) -> Box<dyn Cartridge + 'a> {
    use hardware::flash::FLASH_SECTOR_SIZE;
    device_reset(volume_manager.device());
    let mut volume = volume_manager
        .open_volume(embedded_sdmmc::VolumeIdx(0))
        .unwrap();
    let mut root_dir = volume.open_root_dir().unwrap();
    let mut rom_file = root_dir
        .open_file_in_dir(rom_name, embedded_sdmmc::Mode::ReadOnly)
        .unwrap();

    if ROM_FLASH_SIZE < rom_file.length() as usize {
        panic!(
            "Ram size not bigh enough for Rom of size: {}",
            rom_file.length()
        )
    }
    defmt::info!("Loading rom into flash");
    let mut offsets = rom_file.length() / FLASH_SECTOR_SIZE;
    if rom_file.length() % FLASH_SECTOR_SIZE != 0 {
        offsets += 1;
    }

    let mut buffer = [0u8; FLASH_SECTOR_SIZE as usize];

    let mut loading_screen = LoadingScreen::new(
        Point::new(0, 0),
        Size::new(DISPLAY_HEIGHT as u32, DISPLAY_WIDTH as u32),
        rom_name.to_string(),
    );
    if let Err(_) = loading_screen.draw(display, 0) {};

    for x in 0..offsets {
        defmt::info!("Loading rom into flash for offset: {}", x);
        rom_file.seek_from_start(x * FLASH_SECTOR_SIZE).unwrap();
        rom_file.read(&mut buffer).unwrap();
        let write_result = unsafe { FLASH_ROM_DATA.write_flash(x, &mut buffer) };
        let percent = (x as f32 / offsets as f32) * 100f32;
        defmt::info!(
            "Result from write into flash for offset: {}: {}, percent: {}",
            x,
            write_result,
            percent
        );
        if let Err(_) = loading_screen.update_progress(display, percent as u8) {};
    }

    rom_file.close().unwrap();
    root_dir.close().unwrap();
    volume.close().unwrap();
    defmt::info!("Loading complete");

    let rom_manager = gameboy::static_rom::StaticRomManager::new(
        FLASH_ROM_DATA.read(),
        volume_manager,
        timer,
        device_reset,
    );
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}

#[cfg(feature = "ram_rom")]
#[inline(always)]
fn load_rom<
    'a,
    DISPLAY: DrawTarget<Color = Rgb565>,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    DR: Fn(&mut D) + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    display: &mut DISPLAY,
    volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    rom_name: &str,
    timer: crate::hal::Timer<DT>,
    device_reset: DR,
) -> Box<dyn Cartridge + 'a> {
    defmt::info!("Loading from SDCARD");
    #[const_env::from_env]
    const ROM_CACHE_SIZE: usize = 10;
    let rom_manager: gameboy::rom::SdRomManager<
        D,
        T,
        DT,
        DR,
        ROM_CACHE_SIZE,
        MAX_DIRS,
        MAX_FILES,
        MAX_VOLUMES,
    > = gameboy::rom::SdRomManager::new(rom_name, volume_manager, timer, device_reset);
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}

#[inline(always)]
fn load_boot_rom<
    'a,
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    volume_manager: &'a mut embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Bootrom {
    use gb_core::hardware::boot_rom::BootromData;
    let mut volume0 = volume_manager
        .open_volume(embedded_sdmmc::VolumeIdx(0))
        .unwrap();
    let mut root_dir = volume0.open_root_dir().unwrap();

    if root_dir
        .find_directory_entry(env!("BOOT_ROM_PATH"))
        .is_err()
    {
        return Bootrom::new(None);
    }

    let dmg_boot_bin: &'static mut [u8] = cortex_m::singleton!(: [u8; 0x100]  = [0u8; 0x100 ])
        .unwrap()
        .as_mut_slice();
    let mut boot_rom_file = root_dir
        .open_file_in_dir(env!("BOOT_ROM_PATH"), embedded_sdmmc::Mode::ReadOnly)
        .unwrap();

    boot_rom_file.read(&mut *dmg_boot_bin).unwrap();
    Bootrom::new(Some(BootromData::from_bytes(dmg_boot_bin)))
}

//#[cfg(feature = "psram_rom")]
#[inline(always)]
fn load_rom_to_psram<
    'a,
    DISPLAY: DrawTarget<Color = Rgb565>,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    DR: Fn(&mut D) + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    display: &mut DISPLAY,
    mut volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    timer: crate::hal::Timer<DT>,
    rom_name: &str,
    ram: &'static mut [u8],
    device_reset: DR,
) -> Box<dyn Cartridge + 'a> {
    pub const ROM_READ_BUFFER_SIZE: u32 = 4096 * 4;
    device_reset(volume_manager.device());
    let mut volume = volume_manager
        .open_volume(embedded_sdmmc::VolumeIdx(0))
        .unwrap();
    let mut root_dir = volume.open_root_dir().unwrap();
    let mut rom_file = root_dir
        .open_file_in_dir(rom_name, embedded_sdmmc::Mode::ReadOnly)
        .unwrap();

    if ram.len() < rom_file.length() as usize {
        panic!(
            "Ram size not bigh enough for Rom of size: {}",
            rom_file.length()
        )
    }
    let mut offsets = rom_file.length() / ROM_READ_BUFFER_SIZE;
    if rom_file.length() % ROM_READ_BUFFER_SIZE != 0 {
        offsets += 1;
    }
    defmt::info!("Loading rom into psram");

    let mut buffer = [0u8; ROM_READ_BUFFER_SIZE as usize];

    let mut loading_screen = LoadingScreen::new(
        Point::new(0, 0),
        Size::new(RENDER_WIDTH as u32, RENDER_HEIGHT as u32),
        rom_name.to_string(),
    );
    if let Err(_) = loading_screen.draw(display, 0) {};

    rom_file.seek_from_start(0u32).unwrap();
    for x in 0..offsets {
        defmt::info!("Loading rom into psram for offset: {}", x);
        rom_file.seek_from_start(x * ROM_READ_BUFFER_SIZE).unwrap();
        rom_file.read(&mut buffer).unwrap();
        // let write_result = unsafe { FLASH_ROM_DATA.write_flash(x, &mut buffer) };
        let addr = ROM_READ_BUFFER_SIZE * x;
        ram[addr as usize..addr as usize + ROM_READ_BUFFER_SIZE as usize].copy_from_slice(&buffer);
        let percent = (x as f32 / offsets as f32) * 100f32;
        defmt::info!(
            "Result from write into psram for offset: {}, percent: {}",
            x,
            percent
        );
        if let Err(_) = loading_screen.update_progress(display, percent as u8) {};
    }

    rom_file.close().unwrap();
    root_dir.close().unwrap();
    volume.close().unwrap();
    defmt::info!("Loading complete");

    let rom_manager =
        gameboy::static_rom::StaticRomManager::new(ram, volume_manager, timer, device_reset);
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}
