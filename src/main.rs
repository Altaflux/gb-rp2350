#![no_std]
#![no_main]
#![feature(const_float_bits_conv)]

mod array_scaler;
mod const_math;
mod pio_dma_interface;
mod pio_interface;
mod scaler;
mod stream_display;
mod rp_hal;
use embedded_hal::digital::OutputPin;
extern crate alloc;
use alloc::vec::Vec;
use gb_core::{gameboy::GameBoy, hardware::Screen};
use ili9341::{DisplaySize, DisplaySize240x320};
// Ensure we halt the program on panic (if we don't mention this crate it won't
// be linked)
use panic_halt as _;

use rp_hal::hal::pio::PIOExt;
use rp_hal::hal::dma::DMAExt;
// Alias for our HAL crate
use rp_hal::hal;


// Some things we need
use embedded_alloc::Heap;


/// Tell the Boot ROM about our application
#[link_section = ".start_block"]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

/// External high-speed crystal on the Raspberry Pi Pico 2 board is 12 MHz.
/// Adjust if your board has a different frequency
const XTAL_FREQ_HZ: u32 = 12_000_000u32;

#[global_allocator]
static ALLOCATOR: Heap = Heap::empty();

#[hal::entry]
fn main() -> ! {
    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 131000;
        static mut HEAP: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { ALLOCATOR.init(HEAP.as_ptr() as usize, HEAP_SIZE) }
    }

    // Grab our singleton objects
    let mut pac = hal::pac::Peripherals::take().unwrap();

    // Set up the watchdog driver - needed by the clock setup code
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);

    // Configure the clocks
    let clocks = hal::clocks::init_clocks_and_plls(
        XTAL_FREQ_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .unwrap();

    let mut timer = hal::Timer::new_timer0(pac.TIMER0, &mut pac.RESETS, &clocks);
    let sio = hal::Sio::new(pac.SIO);

    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let mut led_pin = pins.gpio25.into_push_pull_output();

    let reset = pins.gpio2.into_push_pull_output();
    let mut cs = pins.gpio17.into_push_pull_output();
    let rs = pins.gpio28.into_push_pull_output();
    let _rw = pins.gpio22.into_function::<hal::gpio::FunctionPio0>();

    let mut rd = pins.gpio16.into_push_pull_output();

    let _ = pins.gpio6.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio7.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio8.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio9.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio10.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio11.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio12.into_function::<hal::gpio::FunctionPio0>();
    let _ = pins.gpio13.into_function::<hal::gpio::FunctionPio0>();

    let (mut pio, sm0, _, _, _) = pac.PIO0.split(&mut pac.RESETS);
    rd.set_high().unwrap();
    cs.set_low().unwrap();

    let endianess = |be: bool, val: u16| {
        if be {
            val.to_le()
        } else {
            val.to_be()
        }
    };

    let mut interface =
        pio_interface::PioInterface::new(3, rs, &mut pio, sm0, 22, (6, 13), endianess);

    let mut display = ili9341::Ili9341::new_orig(
        interface,
        reset,
        &mut timer,
        ili9341::Orientation::LandscapeFlipped,
        ili9341::DisplaySize240x320,
    )
    .unwrap();

    let gb_rom = load_rom_from_path();
    let cart = gb_rom.into_cartridge();
    let boot_rom = gb_core::hardware::boot_rom::Bootrom::new(Some(
        gb_core::hardware::boot_rom::BootromData::from_bytes(include_bytes!(
            "C:\\roms\\dmg_boot.bin"
        )),
    ));
    let screen = GameboyLineBufferDisplay::new();
    let mut gameboy = GameBoy::create(screen, cart, boot_rom);

    const SCREEN_WIDTH: usize =
        const_math::floorf(<DisplaySize240x320 as DisplaySize>::WIDTH as f32 / 1.0f32) as usize;
    const SCREEN_HEIGHT: usize =
        const_math::floorf(<DisplaySize240x320 as DisplaySize>::HEIGHT as f32 / 1.0f32) as usize;

    let spare: &'static mut [u16] =
        cortex_m::singleton!(: Vec<u16>  = alloc::vec![0; SCREEN_WIDTH ])
            .unwrap()
            .as_mut_slice();

    let dm_spare: &'static mut [u16] =
        cortex_m::singleton!(: Vec<u16>  = alloc::vec![0; SCREEN_WIDTH ])
            .unwrap()
            .as_mut_slice();

    let dma = pac.DMA.split(&mut pac.RESETS);
    let mut streamer = stream_display::Streamer::new(3, 22, (6, 13), dma.ch0, dm_spare, spare);

    // Rectangle::with_center(Point::new(160, 120), Size::new(50, 50))
    //     .into_styled(PrimitiveStyle::with_stroke(Rgb565::CSS_ORANGE_RED, 30))
    //     .draw(&mut display)
    //     .unwrap();

    led_pin.set_high().unwrap();

    loop {

        let display_iter = GameVideoIter::new(&mut gameboy);
        let mut scaler: scaler::ScreenScaler<
            144,
            160,
            { SCREEN_WIDTH },
            { SCREEN_HEIGHT },
            GameVideoIter,
        > = scaler::ScreenScaler::new(display_iter);
        display = display
            .async_transfer_mode(0, 0, SCREEN_HEIGHT as u16, SCREEN_WIDTH as u16, |iface| {
                let (sm, rs) = iface.free(&mut pio);

                let (rs, sm) =
                    streamer.stream::<SCREEN_WIDTH, _, _, _, _>(&mut pio, rs, sm, &mut scaler);

                pio_interface::PioInterface::new(3, rs, &mut pio, sm, 22, (6, 13), endianess)
            })
            .unwrap();

       // hal::arch::wfi();
    }
}

pub struct GameVideoIter<'a> {
    gameboy: &'a mut GameBoy<GameboyLineBufferDisplay>,
    current_line_index: usize,
}
impl<'a> GameVideoIter<'a> {
    fn new(gameboy: &'a mut GameBoy<GameboyLineBufferDisplay>) -> Self {
        Self {
            gameboy: gameboy,
            current_line_index: 0,
        }
    }
}

impl<'a> Iterator for GameVideoIter<'a> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.gameboy.get_screen().turn_off {
                self.gameboy.get_screen().turn_off = false;
                return None;
            }
            if self.gameboy.get_screen().line_complete {
                let pixel = self.gameboy.get_screen().line_buffer[self.current_line_index];
                if self.current_line_index + 1 >= 160 {
                    self.current_line_index = 0;
                    self.gameboy.get_screen().line_complete = false;
                } else {
                    self.current_line_index = self.current_line_index + 1;
                }

                return Some(pixel);
            } else {
                self.gameboy.tick();
            }
        }
    }
}

struct GameboyLineBufferDisplay {
    line_buffer: Vec<u16>,
    line_complete: bool,
    turn_off: bool,
}

impl GameboyLineBufferDisplay {
    fn new() -> Self {
        Self {
            line_buffer: alloc::vec![0; 160],
            line_complete: false,
            turn_off: false,
        }
    }
}

impl Screen for GameboyLineBufferDisplay {
    fn turn_on(&mut self) {
        self.turn_off = true;
    }

    fn turn_off(&mut self) {
        //todo!()
    }

    fn set_pixel(&mut self, x: u8, _y: u8, color: gb_core::hardware::color_palette::Color) {
        let encoded_color = ((color.red as u16 & 0b11111000) << 8)
            + ((color.green as u16 & 0b11111100) << 3)
            + (color.blue as u16 >> 3);

        self.line_buffer[x as usize] = encoded_color;
    }
    fn scanline_complete(&mut self, _y: u8, _skip: bool) {
        self.line_complete = true;
    }

    fn draw(&mut self, _: bool) {}
}

pub fn load_rom_from_path() -> gb_core::hardware::rom::Rom<'static> {
    let rom_f = include_bytes!("C:\\roms\\tetris.gb");
    gb_core::hardware::rom::Rom::from_bytes(rom_f)
}

/// Program metadata for `picotool info`
#[link_section = ".bi_entries"]
#[used]
pub static PICOTOOL_ENTRIES: [hal::binary_info::EntryAddr; 5] = [
    hal::binary_info::rp_cargo_bin_name!(),
    hal::binary_info::rp_cargo_version!(),
    hal::binary_info::rp_program_description!(c"SPI Example"),
    hal::binary_info::rp_cargo_homepage_url!(),
    hal::binary_info::rp_program_build_attribute!(),
];
