use crate::rp_hal::hal;

use hal::pio::UninitStateMachine;
use hal::pio::{PIOExt, PIO};
use hal::pio::{StateMachineIndex, Tx};

use embedded_dma::ReadBuffer;
use hal::dma::double_buffer::{Config as DConfig, Transfer as DTransfer};

use crate::hal::dma::double_buffer::ReadNext;
use hal::dma::ReadTarget;
use hal::dma::SingleChannel;
type ToType<P, SM> = Tx<(P, SM), hal::dma::HalfWord>;
enum DmaState<
    CH1: SingleChannel,
    CH2: SingleChannel,
    FROM: ReadTarget<ReceivedWord = u16>,
    P: PIOExt,
    SM: StateMachineIndex,
> {
    IDLE(DTransfer<CH1, CH2, FROM, ToType<P, SM>, ()>),
    RUNNING(DTransfer<CH1, CH2, FROM, ToType<P, SM>, ReadNext<FROM>>),
}

pub struct I2sPioInterface<CH1: SingleChannel, CH2: SingleChannel, P: PIOExt, SM: StateMachineIndex>
{
    dma_state: Option<DmaState<CH1, CH2, LimitingArrayReadTarget, P, SM>>,
    second_buffer: Option<LimitingArrayReadTarget>,
    sample_rate: u32,
}

impl<CH1, CH2, P, SM> I2sPioInterface<CH1, CH2, P, SM>
where
    CH1: SingleChannel,
    CH2: SingleChannel,
    P: PIOExt,
    SM: StateMachineIndex,
{
    pub fn new(
        sample_rate: u32,
        channel: CH1,
        channel2: CH2,
        clock_divider: (u16, u8),
        pio: &mut PIO<P>,
        sm: UninitStateMachine<(P, SM)>,
        clock_pin: (u8, u8),
        data_pin: u8,
        buffer: &'static mut [u16],
    ) -> Self
    where
        P: PIOExt,
        SM: StateMachineIndex,
    {
        let audio_program = pio::pio_asm!(
            ".side_set 2",
            "    set x, 14          side 0b01", // side 0bWB - W = Word Clock, B = Bit Clock
            "left_data:",
            "    out pins, 1        side 0b00",
            "    jmp x-- left_data  side 0b01",
            "    out pins 1         side 0b10",
            "    set x, 14          side 0b11",
            "right_data:",
            "    out pins 1         side 0b10",
            "    jmp x-- right_data side 0b11",
            "    out pins 1         side 0b00",
        );

        let video_program_installed = pio.install(&audio_program.program).unwrap();
        let (mut video_sm, _rx, vid_tx) =
            hal::pio::PIOBuilder::from_installed_program(video_program_installed)
                .out_pins(data_pin, 1)
                .side_set_pin_base(clock_pin.0)
                .out_shift_direction(hal::pio::ShiftDirection::Left)
                .autopull(true)
                .out_sticky(false)
                .pull_threshold(16)
                .buffers(hal::pio::Buffers::OnlyTx)
                .clock_divisor_fixed_point(clock_divider.0, clock_divider.1)
                .build(sm);
        video_sm.set_pindirs((data_pin..data_pin + 1 as u8).map(|n| (n, hal::pio::PinDir::Output)));
        video_sm.set_pindirs(
            (clock_pin.0..clock_pin.1 + 1 as u8).map(|n| (n, hal::pio::PinDir::Output)),
        );
        let _ = video_sm.start();
        let to_dest = vid_tx.transfer_size(hal::dma::HalfWord);

        let (buffer1, buffer2) = buffer.split_at_mut(buffer.len() / 2);
        let from = LimitingArrayReadTarget::new(buffer1, buffer1.len() as u32);
        let from2 = LimitingArrayReadTarget::new(buffer2, buffer2.len() as u32);
        let cfg = DConfig::new((channel, channel2), from, to_dest).start();

        Self {
            dma_state: Some(DmaState::IDLE(cfg)),
            second_buffer: Some(from2),
            sample_rate: sample_rate,
        }
    }

    fn process_audio(
        output_buffer: &[u16],
        static_buffer: LimitingArrayReadTarget,
    ) -> LimitingArrayReadTarget {
        let output = static_buffer.new_max_read((output_buffer.len() * 1) as u32);
        output.array[..output_buffer.len()].clone_from_slice(output_buffer);
        output
    }
}

impl<CH1, CH2, P, SM> gb_core::hardware::sound::AudioPlayer for I2sPioInterface<CH1, CH2, P, SM>
where
    CH1: SingleChannel,
    CH2: SingleChannel,
    P: PIOExt,
    SM: StateMachineIndex,
{
    fn play(&mut self, output_buffer: &[u16]) {
        let dma_state = core::mem::replace(&mut self.dma_state, None).unwrap();

        match dma_state {
            DmaState::IDLE(transfer) => {
                let second_buffer = core::mem::replace(&mut self.second_buffer, None).unwrap();
                let second_buffer = Self::process_audio(output_buffer, second_buffer);
                let new_transfer = transfer.read_next(second_buffer);
                self.dma_state = Some(DmaState::RUNNING(new_transfer));
            }
            DmaState::RUNNING(transfer) => {
                let dms = transfer.wait();
                let second_buffer = Self::process_audio(output_buffer, dms.0);
                let new_transfer = dms.1.read_next(second_buffer);
                self.dma_state = Some(DmaState::RUNNING(new_transfer));
            }
        };
    }

    fn samples_rate(&self) -> u32 {
        self.sample_rate
    }

    fn underflowed(&self) -> bool {
        // let underflowed = match &self.dma_state {
        //     Some(dma_state) => match dma_state {
        //         DmaState::IDLE(..) => true,
        //         DmaState::RUNNING(transfer) => transfer.is_done(),
        //     },
        //     None => false,
        // };

        true
    }
}

struct LimitingArrayReadTarget {
    array: &'static mut [u16],
    max_read: u32,
}

impl LimitingArrayReadTarget {
    fn new(array: &'static mut [u16], max_read: u32) -> Self {
        Self { array, max_read }
    }

    fn new_max_read(self, max_read: u32) -> Self {
        Self {
            array: self.array,
            max_read,
        }
    }
}

unsafe impl ReadTarget for LimitingArrayReadTarget {
    type ReceivedWord = u16;

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
