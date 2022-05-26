use borsh::{BorshDeserialize, BorshSerialize};

pub trait BorshSerDeSized: BorshSerialize + BorshDeserialize {
    const SIZE: usize;

    fn override_slice(value: &Self, slice: &mut [u8]) {
        let vec = Self::try_to_vec(value).unwrap();
        for i in 0..vec.len() {
            slice[i] = vec[i];
        }
    }
}

pub const fn max(a: usize, b: usize) -> usize {
    [a, b][(a < b) as usize]
}

macro_rules! impl_borsh_sized {
    ($ty: ty, $size: expr) => {
        impl BorshSerDeSized for $ty { const SIZE: usize = $size; }
    };
}

impl<E: BorshSerDeSized + Default + Copy, const N: usize> BorshSerDeSized for [E; N] {
    const SIZE: usize = E::SIZE * N;
}

// Optionals
pub enum ElusivOption<N> {
    Some(N),
    None
}
impl<N: Copy> ElusivOption<N> {
    pub fn unwrap(&self) -> Result<N, ProgramError> {
        match *self {
            ElusivOption::Some(v) => Ok(v),
            ElusivOption::None => Err(ProgramError::InvalidArgument)
        }
    }
}
impl<N: BorshSerDeSized> BorshSerDeSized for ElusivOption<N> {
    const SIZE: usize = N::SIZE + 1;
}
impl<N: BorshDeserialize + BorshSerDeSized> BorshDeserialize for ElusivOption<N> {
    fn deserialize(buf: &mut &[u8]) -> std::io::Result<Self> {
        match bool::deserialize(buf)? {
            true => Ok(ElusivOption::Some(N::deserialize(buf)?)),
            false => Ok(ElusivOption::None),
        }
    }
}
impl<N: BorshSerialize + BorshSerDeSized> BorshSerialize for ElusivOption<N> {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            ElusivOption::Some(v) => {
                writer.write_all(&vec![1])?;
                v.serialize(writer)
            },
            ElusivOption::None => writer.write_all(&vec![0; N::SIZE + 1])
        }
    }
}
/*impl<E: BorshSerDeSized + Default + Copy, const N: usize> BorshSerDeSized for [E; N] {
    const SIZE: usize = E::SIZE * N;
}*/

pub(crate) use impl_borsh_sized;
use solana_program::program_error::ProgramError;

impl_borsh_sized!(u8, 1);
impl_borsh_sized!(u32, 4);
impl_borsh_sized!(u64, 8);
impl_borsh_sized!(bool, 1);

pub fn contains<N: BorshSerialize + BorshSerDeSized>(v: N, data: &[u8]) -> bool {
    let length = data.len() / N::SIZE;
    match find(v, data, length) {
        Some(_) => true,
        None => false
    }
}

pub fn find<N: BorshSerialize + BorshSerDeSized>(v: N, data: &[u8], length: usize) -> Option<usize> {
    let bytes = match N::try_to_vec(&v) {
        Ok(v) => v,
        Err(_) => return None
    };

    assert!(data.len() >= length);
    'A: for i in 0..length {
        let index = i * N::SIZE;
        if data[index] == bytes[0] {
            for j in 1..N::SIZE {
                if data[index + j] != bytes[j] { continue 'A; }
            }
            return Some(i);
        }
    }
    None
}

pub fn is_zero(s: &[u8]) -> bool {
    for i in (0..s.len()).step_by(16) {
        if s.len() - i >= 16 {
            let arr: [u8; 16] = s[i..i+16].try_into().unwrap();
            if u128::from_be_bytes(arr) != 0 { return false }
        } else {
            for i in i..s.len() {
                if s[i] != 0 { return false }
            }
        }
    }
    true
}

pub fn slice_to_array<N: Default + Copy, const SIZE: usize>(s: &[N]) -> [N; SIZE] {
    assert!(s.len() >= SIZE);
    let mut a = [N::default(); SIZE];
    for i in 0..SIZE { a[i] = s[i]; }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::BorshSerDeSized;

    #[test]
    fn test_find_contains() {
        let length = 1000usize;
        let mut data = vec![0; length * 8];
        for i in 0..length {
            let bytes = u64::to_le_bytes(i as u64);
            for j in 0..8 {
                data[i * 8 + j] = bytes[j];
            }
        }

        for i in 0..length {
            assert_eq!(contains(i as u64, &data[..]), true);
            assert_eq!(find(i as u64, &data[..], length).unwrap(), i as usize);
        }
        for i in length..length + 20 {
            assert_eq!(contains(i as u64, &data[..]), false);
            assert!(matches!(find(i as u64, &data[..], length), None));
        }
    }

    #[derive(BorshDeserialize, BorshSerialize)]
    struct A { }
    impl_borsh_sized!(A, 11);

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
    struct B { a0: A, a1: A, a2: A }

    #[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
    enum C {
        A { a: A },
        B { b: B },
        AB { a: A, b: B },
    }

    #[test]
    fn test_borsh_ser_de_sized() {
        assert_eq!(A::SIZE, 11);
        assert_eq!(B::SIZE, 33);
        assert_eq!(C::SIZE, 11 + 33 + 1);
    }
}