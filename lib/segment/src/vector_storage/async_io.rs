use std::fs::File;
use std::os::fd::AsRawFd;

use io_uring::{opcode, types, IoUring};

use crate::common::mmap_ops::transmute_from_u8_to_slice;
use crate::data_types::vectors::VectorElementType;
use crate::entry::entry_point::{OperationError, OperationResult};
use crate::types::PointOffsetType;

const DISK_PARALLELISM: usize = 16; // TODO: benchmark it better, or make it configurable

struct BufferMeta {
    /// Sequential index of the processing point
    pub index: usize,
    /// Id of the point that is currently being processed
    pub point_id: PointOffsetType,
}

struct Buffer {
    /// Stores the buffer for the point vectors
    pub buffer: Vec<u8>,
    /// Stores the point ids that are currently being processed in each buffer.
    pub meta: Option<BufferMeta>,
}

struct BufferStore {
    /// Stores the buffer for the point vectors
    pub buffers: Vec<Buffer>,
}

impl BufferStore {
    pub fn new(num_buffers: usize, buffer_raw_size: usize) -> Self {
        Self {
            buffers: (0..num_buffers)
                .map(|_| Buffer {
                    buffer: vec![0; buffer_raw_size],
                    meta: None,
                })
                .collect(),
        }
    }

    #[allow(dead_code)]
    pub fn new_empty() -> Self {
        Self { buffers: vec![] }
    }
}

pub struct UringReader {
    file: File,
    buffers: BufferStore,
    io_uring: Option<IoUring>,
    raw_size: usize,
    header_size: usize,
}

impl UringReader {
    pub fn new(file: File, raw_size: usize, header_size: usize) -> OperationResult<Self> {
        let buffers = BufferStore::new(DISK_PARALLELISM, raw_size);
        let io_uring = IoUring::new(DISK_PARALLELISM as _)?;

        Ok(Self {
            file,
            buffers,
            io_uring: Some(io_uring),
            raw_size,
            header_size,
        })
    }

    pub fn drop_io_uring(&mut self) {
        self.io_uring = None;
    }

    fn ensure_io_uring(&mut self) -> OperationResult<()> {
        if self.io_uring.is_none() {
            self.io_uring = Some(IoUring::new(DISK_PARALLELISM as _)?);
        }
        Ok(())
    }

    fn submit_and_read(
        &mut self,
        mut callback: impl FnMut(usize, PointOffsetType, &[VectorElementType]),
        unused_buffer_ids: &mut Vec<usize>,
    ) -> OperationResult<()> {
        let buffers_count = self.buffers.buffers.len();
        let used_buffers_count = buffers_count - unused_buffer_ids.len();
        let io_uring = self.io_uring.as_mut().unwrap();

        // Wait for at least one buffer to become available
        io_uring.submit_and_wait(used_buffers_count)?;

        let cqe = io_uring.completion();
        for entry in cqe {
            let result = entry.result();
            if result < 0 {
                return Err(OperationError::service_error(format!(
                    "io_uring operation failed with {} error",
                    result
                )));
            } else if (result as usize) != self.raw_size {
                return Err(OperationError::service_error(format!(
                    "io_uring operation returned {} bytes instead of {}",
                    result, self.raw_size
                )));
            }

            let buffer_id = entry.user_data() as usize;
            let meta = self.buffers.buffers[buffer_id].meta.take().unwrap();
            let buffer = &self.buffers.buffers[buffer_id].buffer;
            let vector = transmute_from_u8_to_slice(buffer);
            callback(meta.index, meta.point_id, vector);
            unused_buffer_ids.push(buffer_id);
        }

        Ok(())
    }

    /// Takes in iterator of point offsets, reads it, and yields a callback with the read data.
    pub fn read_stream(
        &mut self,
        points: impl IntoIterator<Item = PointOffsetType>,
        mut callback: impl FnMut(usize, PointOffsetType, &[VectorElementType]),
    ) -> OperationResult<()> {
        let buffers_count = self.buffers.buffers.len();
        self.ensure_io_uring()?;

        let mut unused_buffer_ids = (0..buffers_count).collect::<Vec<_>>();

        for item in points.into_iter().enumerate() {
            let (idx, point): (usize, PointOffsetType) = item;

            if unused_buffer_ids.is_empty() {
                self.submit_and_read(&mut callback, &mut unused_buffer_ids)?;
            }
            // Assume there is at least one buffer available at this point
            let buffer_id = unused_buffer_ids.pop().unwrap();

            self.buffers.buffers[buffer_id].meta = Some(BufferMeta {
                index: idx,
                point_id: point,
            });

            let buffer = &mut self.buffers.buffers[buffer_id].buffer;
            let offset = self.header_size + self.raw_size * point as usize;

            let user_data = buffer_id;

            let read_e = opcode::Read::new(
                types::Fd(self.file.as_raw_fd()),
                buffer.as_mut_ptr(),
                buffer.len() as _,
            )
            .offset(offset as _)
            .build()
            .user_data(user_data as _);

            unsafe {
                // self.io_uring.submission().push(&read_e).unwrap();
                self.io_uring.as_mut().unwrap().submission().push(&read_e).map_err(|err| {
                    OperationError::service_error(format!("Failed using io-uring: {}", err))
                })?;
            }
        }

        let mut operations_to_wait_for = self.buffers.buffers.len() - unused_buffer_ids.len();

        while operations_to_wait_for > 0 {
            self.submit_and_read(&mut callback, &mut unused_buffer_ids)?;
            operations_to_wait_for = self.buffers.buffers.len() - unused_buffer_ids.len();
        }
        Ok(())
    }
}
