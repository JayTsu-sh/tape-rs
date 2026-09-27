//! 页对齐的用户缓冲区；只调整 Vec 内的切片，不引入裸指针所有权。

const ALIGNMENT: usize = 4096;

pub(crate) struct TransferBuffer {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl TransferBuffer {
    pub(crate) fn new(len: usize) -> Self {
        let storage = vec![0; len.checked_add(ALIGNMENT - 1).expect("传输缓冲区过大")];
        let start = (storage.as_ptr() as usize).wrapping_neg() & (ALIGNMENT - 1);
        Self {
            storage,
            start,
            len,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_transfer_buffers_preserve_length_and_last_byte() {
        for len in [0, 1, 511, 512, 4096, 524288, 524289] {
            let mut buffer = TransferBuffer::new(len);
            let bytes = buffer.as_mut_slice();
            assert_eq!(bytes.len(), len);
            assert_eq!(bytes.as_ptr() as usize % ALIGNMENT, 0);
            if let Some(last) = bytes.last_mut() {
                *last = 0x5a;
                assert_eq!(buffer.as_mut_slice()[len - 1], 0x5a);
            }
        }
    }
}
