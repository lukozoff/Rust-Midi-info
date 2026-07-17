pub struct TrackCursor<'a> { 
    pub data: &'a [u8], 
    pub offset: usize, 
    pub abs_tick: u32, 
    pub track_idx: usize, 
    pub last_status: u8,
    pub end: usize,
}

impl<'a> TrackCursor<'a> {
    #[inline(always)]
    pub fn new(data: &'a [u8], track_idx: usize) -> Self {
        let end = data.len();
        Self { data, offset: 0, abs_tick: 0, track_idx, last_status: 0, end }
    }
}

// Макрос для быстрого чтения varint без граничных проверок (данные гарантированно валидны)
// Возвращает u32 напрямую (без Option) для максимальной скорости
#[macro_export]
macro_rules! read_varint_unchecked {
    ($data:expr, $offset:expr) => {{
        let mut res = 0u32;
        let mut count = 0;
        loop {
            let b = $data[$offset];
            $offset += 1;
            res = (res << 7) | (b & 0x7F) as u32;
            count += 1;
            if b & 0x80 == 0 || count >= 4 { break; }
        }
        res
    }};
}

#[inline(always)]
pub fn read_varint(data: &[u8], offset: &mut usize) -> Option<u32> {
    if *offset >= data.len() { return None; }
    let mut res = 0u32;
    let mut count = 0;
    while *offset < data.len() {
        let b = data[*offset];
        *offset += 1;
        res = (res << 7) | (b & 0x7F) as u32;
        count += 1;
        if b & 0x80 == 0 || count >= 4 { break; }
    }
    Some(res)
}

/// Быстрая версия read_varint_from_cursor с предварительной проверкой границ.
/// Если данных нет — возвращает 0 (в MIDI валидный дельта-тайм не может быть 0xFFFFFFFF,
/// так что 0 сигнализирует о конце трека).
#[inline(always)]
#[allow(dead_code)]
pub fn read_varint_from_cursor(c: &mut TrackCursor) -> u32 {
    if c.offset >= c.end { return 0; }
    read_varint_unchecked!(c.data, c.offset)
}
