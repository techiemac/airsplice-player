//! The boolean entropy decoder (RFC 6386 section 7).

/// Added to the bit count once the input runs out, so the refill is not
/// attempted again for a very long time. Past the end the stream reads as
/// zero bits, as in libvpx.
const LOTS_OF_BITS: i32 = 0x4000_0000;

#[derive(Clone)]
pub struct BoolDecoder<'a> {
    data: &'a [u8],
    /// The window: the top byte takes part in the arithmetic, the bytes below
    /// it are buffered input.
    value: u64,
    /// Always 128..=255 between reads.
    range: u32,
    /// Buffered bits below the top byte; negative when a refill is due.
    count: i32,
}

impl<'a> BoolDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = BoolDecoder { data, value: 0, range: 255, count: -8 };
        d.fill();
        d
    }

    #[cold]
    fn fill_slow(&mut self) {
        let mut shift = 48 - self.count;
        while shift >= 0 {
            let Some((&byte, rest)) = self.data.split_first() else {
                self.count += LOTS_OF_BITS;
                break;
            };
            self.data = rest;
            self.count += 8;
            self.value |= u64::from(byte) << shift;
            shift -= 8;
        }
    }

    #[inline(always)]
    fn fill(&mut self) {
        // count is -8..=-1 here, so 7 whole bytes always fit below what is
        // still buffered. Loading 8 and using 7 keeps this branch-free.
        if let Some((chunk, _)) = self.data.split_first_chunk::<8>() {
            let bits = u64::from_be_bytes(*chunk) >> 8;
            self.value |= bits << (-self.count);
            self.count += 56;
            self.data = &self.data[7..];
        } else {
            self.fill_slow();
        }
    }

    /// Read one bool whose probability of being false is `prob / 256`.
    #[inline(always)]
    pub fn read(&mut self, prob: u8) -> bool {
        let split = 1 + (((self.range - 1) * u32::from(prob)) >> 8);
        if self.count < 0 {
            self.fill();
        }
        let big_split = u64::from(split) << 56;
        let bit = self.value >= big_split;
        if bit {
            self.range -= split;
            self.value -= big_split;
        } else {
            self.range = split;
        }
        // range is 1..=255: shift it back up to at least 128.
        let shift = self.range.leading_zeros() - 24;
        self.range <<= shift;
        self.value <<= shift;
        self.count -= shift as i32;
        bit
    }

    #[inline(always)]
    pub fn read_bit(&mut self) -> bool {
        self.read(128)
    }

    /// An unsigned `bits`-bit number, most significant bit first.
    pub fn read_literal(&mut self, bits: u32) -> u32 {
        let mut v = 0;
        for _ in 0..bits {
            v = (v << 1) | u32::from(self.read(128));
        }
        v
    }

    /// A `bits`-bit magnitude followed by a sign flag.
    pub fn read_signed(&mut self, bits: u32) -> i32 {
        let v = self.read_literal(bits) as i32;
        if self.read_bit() { -v } else { v }
    }

    /// Walk a coding tree (see `tables`) and return the leaf value.
    #[inline(always)]
    pub fn read_tree(&mut self, tree: &[i8], probs: &[u8]) -> u8 {
        let mut i = 0usize;
        loop {
            let bit = usize::from(self.read(probs[i >> 1]));
            let next = tree[i + bit];
            if next <= 0 {
                return (-next) as u8;
            }
            i = next as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward byte-at-a-time decoder of RFC 6386 section 7.3.
    struct Reference<'a> {
        data: &'a [u8],
        value: u32,
        range: u32,
        bit_count: u32,
    }

    impl<'a> Reference<'a> {
        fn new(data: &'a [u8]) -> Self {
            let mut r = Reference { data, value: 0, range: 255, bit_count: 0 };
            r.value = (r.next() << 8) | r.next();
            r
        }
        fn next(&mut self) -> u32 {
            match self.data.split_first() {
                Some((&b, rest)) => {
                    self.data = rest;
                    u32::from(b)
                }
                None => 0,
            }
        }
        fn read(&mut self, prob: u8) -> bool {
            let split = 1 + (((self.range - 1) * u32::from(prob)) >> 8);
            let big = split << 8;
            let bit = self.value >= big;
            if bit {
                self.range -= split;
                self.value -= big;
            } else {
                self.range = split;
            }
            while self.range < 128 {
                self.value <<= 1;
                self.range <<= 1;
                self.bit_count += 1;
                if self.bit_count == 8 {
                    self.bit_count = 0;
                    self.value |= self.next();
                }
            }
            bit
        }
    }

    #[test]
    fn matches_the_spec_decoder() {
        // A fixed pseudo-random stream and probability sequence, read well
        // past the end of the data.
        let mut seed = 0x1234_5678u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        for len in [0usize, 1, 2, 3, 7, 8, 9, 15, 16, 17, 100, 1000] {
            let data: Vec<u8> = (0..len).map(|_| rand()).collect();
            let mut fast = BoolDecoder::new(&data);
            let mut slow = Reference::new(&data);
            for i in 0..len * 12 + 200 {
                let prob = rand();
                assert_eq!(fast.read(prob), slow.read(prob), "len {len} read {i}");
            }
        }
    }
}
