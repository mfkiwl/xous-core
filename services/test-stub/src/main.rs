#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

use core::{
    fmt::Debug, hash::Hash, ptr::copy_nonoverlapping,
};

macro_rules! unsafe_read_slice {
    ($src:expr, $dst:expr, $size:expr, $which:ident) => {{
        assert_eq!($src.len(), $size * $dst.len());

        unsafe {
            copy_nonoverlapping(
                $src.as_ptr(),
                $dst.as_mut_ptr() as *mut u8,
                $src.len(),
            );
        }
        for v in $dst.iter_mut() {
            *v = v.$which();
        }
    }};
}
pub trait ByteOrder:
{
    fn read_u64_into(src: &[u8], dst: &mut [u64]);
}
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LittleEndian {}

impl Default for LittleEndian {
    fn default() -> LittleEndian {
        panic!("LittleEndian default")
    }
}
impl ByteOrder for LittleEndian {
    #[inline]
    fn read_u64_into(src: &[u8], dst: &mut [u64]) {
        unsafe_read_slice!(src, dst, 8, to_le);
    }
}

#[derive(Copy, Clone, Hash)]
pub struct Scalar {
    pub bytes: [u8; 32],
}
impl Scalar {
    pub fn non_adjacent_form(&self, w: usize) -> [i8; 256] {

        let mut naf = [0i8; 256];

        let mut x_u64 = [0u64; 5];
        LittleEndian::read_u64_into(&self.bytes, &mut x_u64[0..4]);

        let width = 1 << w;
        let window_mask = width - 1;

        let mut pos = 0;
        let mut carry = 0;
        while pos < 256 {
            //log::info!("naf[{}]: {:?}", pos, naf);

            // Construct a buffer of bits of the scalar, starting at bit `pos`
            let u64_idx = pos / 64;
            let bit_idx = pos % 64;
            let bit_buf: u64;
            if bit_idx < 64 - w {
                // This window's bits are contained in a single u64
                bit_buf = x_u64[u64_idx] >> bit_idx;
            } else {
                // Combine the current u64's bits with the bits from the next u64
                bit_buf = (x_u64[u64_idx] >> bit_idx) | (x_u64[1+u64_idx] << (64 - bit_idx));
            }

            // Add the carry into the current window
            let window = carry + (bit_buf & window_mask);

            if window & 1 == 0 {
                // If the window value is even, preserve the carry and continue.
                // Why is the carry preserved?
                // If carry == 0 and window & 1 == 0, then the next carry should be 0
                // If carry == 1 and window & 1 == 0, then bit_buf & 1 == 1 so the next carry should be 1
                pos += 1;
                continue;
            }

            if window < width/2 {
                carry = 0;
                naf[pos] = window as i8;
            } else {
                carry = 1;
                naf[pos] = (window as i8).wrapping_sub(width as i8);
            }

            pos += w;
        }

        naf
    }
}

#[xous::xous_main]
fn xmain() -> ! {
    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    let scalar = Scalar {
        bytes: [189, 59, 214, 8, 77, 86, 240, 50, 111, 170, 86, 37, 124, 154, 209, 79, 102, 72, 93, 53, 130, 157, 102, 200, 60, 240, 215, 104, 246, 58, 214, 13],
    };
    let a_naf = scalar.non_adjacent_form(5);

    let expected_result = [-3, 0, 0, 0, 0, 0, 15, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 11, 0, 0, 0, 0, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0, 13, 0, 0, 0, 0, 0, -7, 0, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 15, 0, 0, 0, 0, -7, 0, 0, 0, 0, -3, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, -11, 0, 0, 0, 0, -5, 0, 0, 0, 0, 11, 0, 0, 0, 0, 5, 0, 0, 0, 0, 1, 0, 0, 0, 0, -1, 0, 0, 0, 0, -11, 0, 0, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, -3, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, -13, 0, 0, 0, 0, 0, -15, 0, 0, 0, 0, -11, 0, 0, 0, 0, 15, 0, 0, 0, 0, -11, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, -5, 0, 0, 0, 0, 0, -11, 0, 0, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, -7, 0, 0, 0, 0, -3, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, -1, 0, 0, 0, 0, 0, 0, -5, 0, 0, 0, 0, 9, 0, 0, 0, 0, -13, 0, 0, 0, 0, 0, -1, 0, 0, 0, 0, -5, 0, 0, 0, 0, 0, -7, 0, 0, 0, 0, -5, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0];

    log::info!("a_naf: {:?}", a_naf);
    log::info!("expected_result: {:?}", expected_result);
    assert!(a_naf == expected_result, "mismatch!");

    loop {
        xous::yield_slice();
    }
    /*
    log::trace!("quitting");
    xous::terminate_process(0)
    */
}
