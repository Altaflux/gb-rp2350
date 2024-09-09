//New Scaler
use alloc::vec::Vec;

use crate::const_math::ceilf;

type CalcsType = u16;

pub struct ScreenScaler<
    const IN_HEIGHT: usize,
    const IN_WIDTH: usize,
    const OUT_HEIGHT: usize,
    const OUT_WIDTH: usize,
    I: Iterator<Item = u16>,
> {
    iterator: I,
    input_current_scan_line: u16,
    output_current_scan_line: u16,
    scaled_scan_line_buffer: Vec<u16>,
    scaled_line_buffer_repeat: u16,
    current_scaled_line_index: u16,
    last_x_position: u16,
}

impl<
        const IN_HEIGHT: usize,
        const IN_WIDTH: usize,
        const OUT_HEIGHT: usize,
        const OUT_WIDTH: usize,
        I,
    > ScreenScaler<IN_HEIGHT, IN_WIDTH, OUT_HEIGHT, OUT_WIDTH, I>
where
    I: Iterator<Item = u16>,
{
    const WIDTH_CEIL_CALCS: [CalcsType; OUT_WIDTH] =
        gen_ceil_array(OUT_WIDTH as f32 / IN_WIDTH as f32);
    const HEIGHT_CEIL_CALCS: [CalcsType; OUT_HEIGHT] =
        gen_ceil_array(OUT_HEIGHT as f32 / IN_HEIGHT as f32);

    pub fn new(iterator: I) -> Self {
        Self {
            iterator: iterator,
            input_current_scan_line: 0,
            output_current_scan_line: 0,
            scaled_scan_line_buffer: alloc::vec![0; OUT_WIDTH],
            scaled_line_buffer_repeat: 0,
            current_scaled_line_index: 0,
            last_x_position: 0,
        }
    }
}

impl<
        I,
        const IN_HEIGHT: usize,
        const IN_WIDTH: usize,
        const OUT_HEIGHT: usize,
        const OUT_WIDTH: usize,
    > Iterator for ScreenScaler<IN_HEIGHT, IN_WIDTH, OUT_HEIGHT, OUT_WIDTH, I>
where
    I: Iterator<Item = u16>,
{
    type Item = u16;
    //#[unroll::unroll_for_loops]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.scaled_line_buffer_repeat > 0 {
                let pixel = self.scaled_scan_line_buffer[self.current_scaled_line_index as usize];

                let next_current_scaled_line_index = self.current_scaled_line_index + 1;
                if next_current_scaled_line_index < OUT_WIDTH as u16 {
                    self.current_scaled_line_index = next_current_scaled_line_index;
                } else {
                    self.scaled_line_buffer_repeat -= 1;
                    self.current_scaled_line_index = 0;
                }
                return Some(pixel);
            }

            //Collect all pixes from a scan line
            for count in 0..IN_WIDTH {
                let pixel = self.iterator.next();
                if pixel.is_none() {
                    return None;
                }

                let last_pixel = Self::WIDTH_CEIL_CALCS[count] as u16;
                self.scaled_scan_line_buffer[(self.last_x_position as usize)..last_pixel as usize]
                    .fill(pixel.unwrap());

                self.last_x_position = last_pixel;
            }
            self.last_x_position = 0;

            //Calculate y position of the next scan line
            let next_scan_line_start =
                Self::HEIGHT_CEIL_CALCS[(self.input_current_scan_line + 1) as usize] as u16;
            //How many scan lines are in bewteen the previous last scan line and the next, this is the amount of scan line repetitions needed for Y scaling

            self.scaled_line_buffer_repeat =
                (next_scan_line_start - self.output_current_scan_line) - 0;

            self.output_current_scan_line = next_scan_line_start;

            if self.input_current_scan_line >= IN_HEIGHT as u16 - 1 {
                self.output_current_scan_line = 0;
                self.input_current_scan_line = 0;
            } else {
                self.input_current_scan_line += 1;
            }
        }
    }
}

const fn gen_ceil_array<const N: usize>(ratio: f32) -> [CalcsType; N] {
    let mut res = [0 as CalcsType; N];

    let mut i = 0;

    while i < N as i32 {
        res[i as usize] = ceilf(ratio * i as f32) as CalcsType;
        i += 1;
    }

    res
}
