use crate::rp_hal::hal;
use alloc::boxed::Box;
use defmt::info;
use display_interface::{DataFormat, DisplayError, WriteOnlyDataCommand};
use embedded_hal::digital::OutputPin;

use fon::chan::{Ch16, Ch32};
use fon::{Audio, Frame};
use hal::pio::{PIOExt, PIO};
use hal::pio::{Running, StateMachine, StateMachineIndex, Tx};
use hal::pio::{Rx, UninitStateMachine};
type Result = core::result::Result<(), DisplayError>;

use embedded_dma::{ReadBuffer, Word};
use hal::dma::single_buffer::{Config, Transfer};
use hal::dma::Byte;
use hal::dma::HalfWord;
use hal::dma::SingleChannel;
use hal::dma::{ReadTarget, WriteTarget};
use rp235x_hal::dma::Pace;

// use defmt::*;
// use defmt_rtt as _;

type ToType<P, SM> = Tx<(P, SM), hal::dma::Word>;
enum DmaState<
    CH: SingleChannel,
    FROM: ReadTarget<ReceivedWord = u32>,
    P: PIOExt,
    SM: StateMachineIndex,
> {
    IDLE(CH, FROM, ToType<P, SM>),
    RUNNING(Transfer<CH, FROM, ToType<P, SM>>),
}

pub struct I2sPioInterface<CH: SingleChannel, P: PIOExt, SM: StateMachineIndex> {
    dma_state: Option<DmaState<CH, LimitingArrayReadTarget, P, SM>>,
}

impl<CH, P, SM> I2sPioInterface<CH, P, SM>
where
    CH: SingleChannel,
    P: PIOExt,
    SM: StateMachineIndex,
{
    pub fn new(
        channel: CH,
        clock_divider: (u16, u8),
        pio: &mut PIO<P>,
        sm: UninitStateMachine<(P, SM)>,
        clock_pin: (u8, u8),
        data_pin: u8,
        buffer: &'static mut [u32],
    ) -> Self
    where
        P: PIOExt,
        SM: StateMachineIndex,
    {
        //info!("With clock divider of: {}", clock_divider);
        let audio_program = pio_proc::pio_file!("src/audio_i2s.pio");

        let video_program_installed = pio.install(&audio_program.program).unwrap();
        let program_offset = video_program_installed.offset();

        let (mut video_sm, rx, vid_tx) =
            hal::pio::PIOBuilder::from_installed_program(video_program_installed)
                .out_pins(data_pin, 1)
                .side_set_pin_base(clock_pin.0)
                .out_shift_direction(hal::pio::ShiftDirection::Left)
                .autopull(true)
                .pull_threshold(32)
                .buffers(hal::pio::Buffers::OnlyTx)
                .clock_divisor_fixed_point(clock_divider.0, clock_divider.1)
                .build(sm);
        video_sm.set_pindirs((data_pin..data_pin + 1 as u8).map(|n| (n, hal::pio::PinDir::Output)));
        video_sm.set_pindirs(
            (clock_pin.0..clock_pin.1 + 1 as u8).map(|n| (n, hal::pio::PinDir::Output)),
        );
        let mut sm = video_sm.start();
        let to_dest = vid_tx.transfer_size(hal::dma::Word);

        let instruction = pio::Instruction {
            operands: pio::InstructionOperands::JMP {
                condition: pio::JmpCondition::Always,
                address: program_offset + audio_program.public_defines.entry_point as u8,
            },
            delay: 0,
            side_set: Some(0b00),
        };
        sm.exec_instruction(instruction);
        Self {
            dma_state: Some(DmaState::IDLE(
                channel,
                LimitingArrayReadTarget::new(buffer, buffer.len() as u32),
                // buffer,
                to_dest,
            )),
        }
    }

    // #[allow(dead_code)]
    // pub fn free(self, pio: &mut PIO<P>) -> (UninitStateMachine<(P, SM)>, RS) {
    //     let (sm, prg) = self.sm.uninit(self.rx, self.tx);
    //     pio.uninstall(prg);
    //     (sm, self.rs)
    // }
}

impl<CH, P, SM> gb_core::hardware::sound::AudioPlayer for I2sPioInterface<CH, P, SM>
where
    CH: SingleChannel,
    P: PIOExt,
    SM: StateMachineIndex,
{
    fn play(&mut self, output_buffer: &[f32]) {
        let dma_state = core::mem::replace(&mut self.dma_state, None).unwrap();
        let (ch, audio_buffer, tx) = match dma_state {
            DmaState::IDLE(ch, buff, tx) => (ch, buff, tx),
            DmaState::RUNNING(dma) => dma.wait(),
        };
        let output = audio_buffer.new_max_read((output_buffer.len() * 8) as u32);

        for (i, v) in output_buffer.chunks(2).enumerate() {
            let frame: fon::Frame<fon::chan::Ch16, 2> = fon::Frame::<_, 2>::new(
                fon::chan::Ch16::from(Ch32::new(v[0])),
                fon::chan::Ch16::from(Ch32::new(v[1])),
            );

            let channels = frame.channels();
            let ch1: i16 = channels[0].into();
            let ch2: i16 = channels[1].into();

            let combined = combine_u16_to_u32(ch1 as u16, ch2 as u16);

            output.array[(i * 8) + 0] = combined;
            output.array[(i * 8) + 1] = combined;
            output.array[(i * 8) + 2] = combined;
            output.array[(i * 8) + 3] = combined;
            output.array[(i * 8) + 4] = combined;
            output.array[(i * 8) + 5] = combined;
            output.array[(i * 8) + 6] = combined;
            output.array[(i * 8) + 7] = combined;
        }
        info!("Array size:{}", output_buffer.len());

        let mut sbc = Config::new(ch, output, tx);
        sbc.pace(Pace::PreferSink);
        self.dma_state = Some(DmaState::RUNNING(sbc.start()));
    }

    fn samples_rate(&self) -> u32 {
        5512
    }

    fn underflowed(&self) -> bool {
        let blocked = match &self.dma_state {
            Some(dma_state) => match dma_state {
                DmaState::IDLE(..) => true,
                DmaState::RUNNING(transfer) => transfer.is_done(),
            },
            None => false,
        };
        if !blocked {
            info!("Was blocked");
        }

        true
    }
}

struct LimitingArrayReadTarget {
    array: &'static mut [u32],
    max_read: u32,
}

impl LimitingArrayReadTarget {
    fn new(array: &'static mut [u32], max_read: u32) -> Self {
        Self { array, max_read }
    }

    fn new_max_read(self, max_read: u32) -> Self {
        Self {
            array: self.array,
            max_read,
        }
    }
}
fn convert_sampling(i: f32) -> i16 {
    let clamped = (i).max(-1.0).min(1.0);
    let clamp = (clamped * i16::MAX as f32) as i16;
    clamp
}
fn combine_u16_to_u32(high: u16, low: u16) -> u32 {
    // Shift the high value to the left by 16 bits and combine with low
    ((high as u32) << 16) | (low as u32)
}
unsafe impl ReadTarget for LimitingArrayReadTarget {
    type ReceivedWord = u32;

    fn rx_treq() -> Option<u8> {
        None
    }

    fn rx_address_count(&self) -> (u32, u32) {
        let (ptr, _) = unsafe { self.array.read_buffer() };
        (ptr as u32, self.max_read as u32)
    }

    fn rx_increment(&self) -> bool {
        self.array.rx_increment()
    }
}
