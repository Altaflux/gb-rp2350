#![no_std]
#![no_main]

mod clocks;

mod gameboy;
mod hardware;

mod rp_hal;

mod util;

use alloc::boxed::Box;
use cortex_m::asm;
use embedded_hal::digital::OutputPin;
use embedded_sdmmc::sdcard::AcquireOpts;
use gb_core::hardware::boot_rom::Bootrom;
use gb_core::hardware::cartridge::Cartridge;
use mipidsi::options::{Orientation, Rotation};
use panic_probe as _;
extern crate alloc;

use embedded_sdmmc::{SdCard, VolumeManager};
use gameboy::display::GameboyLineBufferDisplay;
use gameboy::{GameEmulationHandler, InputButtonMapper};
use gb_core::gameboy::GameBoy;
use hal::fugit::RateExtU32;

use hardware::display::ScreenScaler;

use rp235x_hal::gpio::{FunctionSio, SioOutput};
use rp235x_hal::timer::TimerDevice;
use rp235x_hal::uart::{DataBits, StopBits, UartConfig};
use rp235x_hal::{spi, Clock};
use rp_hal::hal::dma::DMAExt;
use rp_hal::hal::pio::PIOExt;

// Alias for our HAL crate
use rp_hal::hal;
// Some things we need
use embedded_alloc::LlffHeap as Heap;

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

#[const_env::from_env]
const DISPLAY_WIDTH: u16 = 240;
#[const_env::from_env]
const DISPLAY_HEIGHT: u16 = 320;
#[const_env::from_env]
const GAMEBOY_RENDER_WIDTH: u16 = 240;
#[const_env::from_env]
const GAMEBOY_RENDER_HEIGHT: u16 = 320;

#[const_env::from_env]
const ENABLE_PSRAM: bool = true;

#[hal::entry]
fn main() -> ! {
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
        asm::nop();
    }
    pac.POWMAN
        .vreg()
        .modify(|_, w| unsafe { w.bits(0x5AFE_0000).vsel().bits(0b01111) }); // 0b01111 = 1.30V

    while pac.POWMAN.vreg().read().update_in_progress().bit_is_set() {
        asm::nop();
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

    #[cfg(feature = "psram")]
    let psram_size = {
        let _ = pin_into_function!(pins, env!("PIN_PSRAM_CS"), hal::gpio::FunctionXipCs1);
        hardware::psram::psram_init(
            clocks.peripheral_clock.freq().to_Hz(),
            &pac.QMI,
            &pac.XIP_CTRL,
        )
    };
    #[cfg(not(feature = "psram"))]
    let psram_size = 0;

    let mut timer: rp235x_hal::Timer<rp235x_hal::timer::CopyableTimer0> =
        hal::Timer::new_timer0(pac.TIMER0, &mut pac.RESETS, &clocks);

    ///////////////////UART!
    let uart0_pins = (
        pin_into_function!(pins, env!("PIN_UART_RX")),
        pin_into_function!(pins, env!("PIN_UART_TX")),
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

        //  unsafe { ALLOCATOR.init(0x11000000 as usize, HEAP_SIZE) }
    }

    let mut led_pin = pins.gpio25.into_push_pull_output();

    let (mut pio_0, sm0_0, sm0_1, _, _) = pac.PIO0.split(&mut pac.RESETS);

    let (mut pio_1, sm_1_0, _, _, _) = pac.PIO1.split(&mut pac.RESETS);
    let dma = pac.DMA.split(&mut pac.RESETS);

    ///////////////////////////////SD CARD
    let spi_sclk: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_into_function!(pins, env!("PIN_SD_CARD_SCLK"), hal::gpio::FunctionSpi);

    let spi_mosi: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_into_function!(pins, env!("PIN_SD_CARD_MOSI"), hal::gpio::FunctionSpi);
    let spi_cs = pin_into_function!(pins, env!("PIN_SD_CARD_CS"), FunctionSio<SioOutput>);
    let spi_miso: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pin_into_function!(pins, env!("PIN_SD_CARD_MISO"), hal::gpio::FunctionSpi);

    // Create the SPI driver instance for the SPI0 device
    let spi = spi::Spi::<_, _, _, 8>::new(pac.SPI1, (spi_mosi, spi_miso, spi_sclk));
    let spi = spi.init(
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
        400.kHz(),
        embedded_hal::spi::MODE_0,
    );

    defmt::info!("PSRAM initialized");

    let exclusive_spi = embedded_hal_bus::spi::ExclusiveDevice::new_no_delay(spi, spi_cs).unwrap();
    let sdcard = SdCard::new_with_options(
        exclusive_spi,
        timer,
        AcquireOpts {
            acquire_retries: 100,
            use_crc: true,
        },
    );
    let mut volume_mgr: VolumeManager<_, _, 3, 2, 1> =
        VolumeManager::new_with_limits(sdcard, hardware::sdcard::DummyTimesource::default(), 5000);

    let boot_rom = load_boot_rom(&mut volume_mgr);
    let cartridge = if cfg!(feature = "sdcard_rom") && psram_size > 0 {
        defmt::info!("Using PRSAM");

        let psram = unsafe {
            const PSRAM_ADDRESS: usize = 0x11000000;

            let ptr = PSRAM_ADDRESS as *mut u8; // Using u8 for byte array
            let slice: &'static mut [u8] =
                alloc::slice::from_raw_parts_mut(ptr, psram_size as usize);
            slice
        };
        let cartridge = load_rom_to_psram(volume_mgr, timer, psram);
        cartridge
    } else {
        let cartridge = load_rom(volume_mgr, timer);
        cartridge
    };

    //////////////////////AUDIO SETUP

    let sample_rate: u32 = 16_000;
    let clock_divider: u32 = clocks.system_clock.freq().to_Hz() * 4 / sample_rate;

    let int_divider = (clock_divider >> 8) as u16;
    let frak_divider = (clock_divider & 0xFF) as u8;

    let i2s_din = pin_into_function!(pins, env!("PIN_I2S_DIN"), hal::gpio::FunctionPio1);
    let i2s_bclk = pin_into_function!(pins, env!("PIN_I2S_BCLK"), hal::gpio::FunctionPio1);
    let i2s_lrc = pin_into_function!(pins, env!("PIN_I2S_LRC"), hal::gpio::FunctionPio1);
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
    let mut gameboy = GameBoy::create(screen, cartridge, boot_rom, Box::new(i2s_interface));

    //SCREEN
    let screen_data_command_pin = pins.gpio7.into_push_pull_output();

    let spi_sclk: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pins.gpio4.into_function::<hal::gpio::FunctionPio0>();
    let spi_mosi: hal::gpio::Pin<_, _, hal::gpio::PullDown> =
        pins.gpio5.into_function::<hal::gpio::FunctionPio0>();

    let display_buffer: &'static mut [u16] =
        cortex_m::singleton!(: [u16;(GAMEBOY_RENDER_WIDTH as usize) * 3]  = [0u16; (GAMEBOY_RENDER_WIDTH as usize ) * 3 ])
            .unwrap()
            .as_mut_slice();

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
    );

    let display_reset = pins.gpio8.into_push_pull_output();

    let mut display = mipidsi::Builder::new(mipidsi::models::ILI9341Rgb565, display_interface)
        .reset_pin(display_reset)
        .display_size(DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16)
        .orientation(Orientation {
            rotation: Rotation::Deg90,
            mirrored: true,
        })
        .init(&mut timer)
        .unwrap();

    let scaler: ScreenScaler<
        144,
        160,
        { GAMEBOY_RENDER_WIDTH as usize },
        { GAMEBOY_RENDER_HEIGHT as usize },
    > = ScreenScaler::new();

    ////////////////////// JOYPAD
    let mut b_button = pins.gpio16.into_pull_up_input().into_dyn_pin();
    let mut a_button = pins.gpio17.into_pull_up_input().into_dyn_pin();
    let mut right_button = pins.gpio18.into_pull_up_input().into_dyn_pin();
    let mut down_button = pins.gpio19.into_pull_up_input().into_dyn_pin();
    let mut left_button = pins.gpio20.into_pull_up_input().into_dyn_pin();
    let mut up_button = pins.gpio21.into_pull_up_input().into_dyn_pin();
    let mut select_button = pins.gpio22.into_pull_up_input().into_dyn_pin();
    let mut start_button = pins.gpio26.into_pull_up_input().into_dyn_pin();
    let mut button_handler = InputButtonMapper::new(
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

    let mut loop_counter: usize = 0;

    const MIDDLE_HEIGHT: u16 = (DISPLAY_HEIGHT - GAMEBOY_RENDER_HEIGHT) / 2;
    const MIDDLE_WIDTH: u16 = (DISPLAY_WIDTH - GAMEBOY_RENDER_WIDTH) / 2;
    loop {
        defmt::info!("Free Mem: {}", ALLOCATOR.free());
        defmt::info!("Used Mem: {}", ALLOCATOR.used());

        let start_time = timer.get_counter();

        display
            .set_pixels(
                MIDDLE_HEIGHT,
                MIDDLE_WIDTH,
                (GAMEBOY_RENDER_HEIGHT - 1) as u16 + MIDDLE_HEIGHT,
                (GAMEBOY_RENDER_WIDTH - 1) as u16 + MIDDLE_WIDTH,
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

#[cfg(feature = "sdcard_rom")]
#[inline(always)]
fn load_rom<
    'a,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    timer: crate::hal::Timer<DT>,
) -> Box<dyn Cartridge + 'a> {
    defmt::info!("Loading from SDCARD");
    #[const_env::from_env]
    const ROM_CACHE_SIZE: usize = 10;
    let rom_manager: gameboy::rom::SdRomManager<
        D,
        T,
        DT,
        ROM_CACHE_SIZE,
        MAX_DIRS,
        MAX_FILES,
        MAX_VOLUMES,
    > = gameboy::rom::SdRomManager::new(env!("ROM_PATH"), volume_manager, timer);
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}

#[cfg(feature = "flash_rom")]
#[inline(always)]
fn load_rom<
    'a,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    timer: crate::hal::Timer<DT>,
) -> Box<dyn Cartridge + 'a> {
    let game_rom = include_bytes!(env!("ROM_PATH"));
    let rom_manager = gameboy::static_rom::StaticRomManager::new(game_rom, volume_manager, timer);
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}

#[cfg(feature = "boot_sdcard_rom")]
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

    let dmg_boot_bin: &'static mut [u8] = cortex_m::singleton!(: [u8; 0x100]  = [0u8; 0x100 ])
        .unwrap()
        .as_mut_slice();
    let mut boot_rom_file = root_dir
        .open_file_in_dir(env!("BOOT_ROM_PATH"), embedded_sdmmc::Mode::ReadOnly)
        .unwrap();

    boot_rom_file.read(&mut *dmg_boot_bin).unwrap();
    Bootrom::new(Some(BootromData::from_bytes(dmg_boot_bin)))
}

#[inline(always)]
fn load_rom_to_psram<
    'a,
    D: embedded_sdmmc::BlockDevice + 'a,
    T: embedded_sdmmc::TimeSource + 'a,
    DT: TimerDevice + 'a,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    mut volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    timer: crate::hal::Timer<DT>,
    ram: &'static mut [u8],
) -> Box<dyn Cartridge + 'a> {
    let rom_name = env!("ROM_PATH");
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
    defmt::info!("Loading rom into psram");
    rom_file.seek_from_start(0u32).unwrap();
    rom_file.read(ram).unwrap();
    rom_file.close().unwrap();
    root_dir.close().unwrap();
    volume.close().unwrap();
    defmt::info!("Loading complete");

    let rom_manager = gameboy::static_rom::StaticRomManager::new(ram, volume_manager, timer);
    let gb_rom = gb_core::hardware::rom::Rom::from_bytes(rom_manager);
    gb_rom.into_cartridge()
}

#[cfg(feature = "boot_flash_rom")]
#[inline(always)]
fn load_boot_rom<
    'a,
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    _volume_manager: &'a mut embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Bootrom {
    use gb_core::hardware::boot_rom::BootromData;
    let game_rom = include_bytes!(env!("BOOT_ROM_PATH"));

    let dmg_boot_bin: &'static mut [u8] = cortex_m::singleton!(: [u8; 0x100]  = [0u8; 0x100 ])
        .unwrap()
        .as_mut_slice();
    dmg_boot_bin.copy_from_slice(game_rom);
    Bootrom::new(Some(BootromData::from_bytes(dmg_boot_bin)))
}

#[cfg(feature = "boot_none_rom")]
#[inline(always)]
fn load_boot_rom<
    'a,
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    _volume_manager: &'a mut embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Bootrom {
    Bootrom::new(None)
}
