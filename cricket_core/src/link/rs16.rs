//! Reed–Solomon codec over GF(16) — one code symbol per MFSK-16 tone (nibble).
//!
//! Field: GF(2^4) with primitive polynomial `x^4 + x + 1` (0x13), α = 2.
//! Code: narrow-sense RS, generator `g(x) = Π_{i=1..=nsym} (x − α^i)`,
//! shortened to length `n = k + nsym <= 15`. Systematic.
//!
//! **Wire order:** a codeword is `k` data nibbles followed by `nsym` parity
//! nibbles. Wire index `w` is the coefficient of `x^(n−1−w)`, i.e. the first
//! symbol sent is the highest-degree coefficient.
//!
//! Decoding corrects `e` errors and `f` erasures whenever `2e + f <= nsym`
//! (errors-and-erasures: Forney syndromes + Berlekamp–Massey + Chien +
//! Forney's formula). Table-free apart from two 16/32-byte tables, no
//! allocation beyond small `Vec`s; `no_std` + `alloc`.

use alloc::vec;
use alloc::vec::Vec;

/// Maximum codeword length over GF(16).
pub const RS16_MAX_N: usize = 15;

const PRIM: u8 = 0x13;

const fn build_tables() -> ([u8; 32], [u8; 16]) {
    let mut exp = [0u8; 32];
    let mut log = [0u8; 16];
    let mut x: u8 = 1;
    let mut i = 0;
    while i < 15 {
        exp[i] = x;
        exp[i + 15] = x;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x10 != 0 {
            x ^= PRIM;
        }
        i += 1;
    }
    exp[30] = exp[0];
    exp[31] = exp[1];
    (exp, log)
}

const TABLES: ([u8; 32], [u8; 16]) = build_tables();
const EXP: [u8; 32] = TABLES.0;
const LOG: [u8; 16] = TABLES.1;

#[inline]
fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        EXP[LOG[a as usize] as usize + LOG[b as usize] as usize]
    }
}

#[inline]
fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    EXP[(15 - LOG[a as usize] as usize) % 15]
}

/// α^e for any integer exponent.
#[inline]
fn alpha_pow(e: i32) -> u8 {
    EXP[e.rem_euclid(15) as usize]
}

/// Evaluate a low-degree-first polynomial at `x`.
fn eval(poly: &[u8], x: u8) -> u8 {
    let mut acc = 0u8;
    for &c in poly.iter().rev() {
        acc = mul(acc, x) ^ c;
    }
    acc
}

/// Product of two low-degree-first polynomials.
fn poly_mul(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; a.len() + b.len() - 1];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 {
            continue;
        }
        for (j, &y) in b.iter().enumerate() {
            out[i + j] ^= mul(x, y);
        }
    }
    out
}

/// Generator polynomial, highest-degree first (monic): `[1, g1, .., g_nsym]`.
fn generator(nsym: usize) -> Vec<u8> {
    // Build low-first then reverse.
    let mut g = vec![1u8];
    for i in 1..=nsym {
        // (x + α^i) low-first = [α^i, 1]
        g = poly_mul(&g, &[alpha_pow(i as i32), 1]);
    }
    g.reverse();
    g
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rs16Error {
    /// More errata than the code can correct (or a miscorrection was detected).
    Uncorrectable,
    /// Bad parameters (`n > 15`, `nsym == 0` or `nsym >= n`, wrong lengths).
    BadParams,
}

/// A shortened RS(n, k) code over GF(16) with `nsym = n − k` parity nibbles.
#[derive(Debug, Clone)]
pub struct Rs16 {
    n: usize,
    nsym: usize,
    gen: Vec<u8>,
}

impl Rs16 {
    /// RS(`k + nsym`, `k`). `Err(BadParams)` for `k == 0`, `nsym == 0` or
    /// `k + nsym > 15`.
    pub fn new(k: usize, nsym: usize) -> Result<Self, Rs16Error> {
        if k == 0 || nsym == 0 || k + nsym > RS16_MAX_N {
            return Err(Rs16Error::BadParams);
        }
        Ok(Self {
            n: k + nsym,
            nsym,
            gen: generator(nsym),
        })
    }

    pub fn n(&self) -> usize {
        self.n
    }

    pub fn k(&self) -> usize {
        self.n - self.nsym
    }

    pub fn nsym(&self) -> usize {
        self.nsym
    }

    /// Systematic encode: `data` (k nibbles) → codeword (n nibbles, data then parity).
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        assert_eq!(data.len(), self.k(), "RS16 data length");
        let mut rem = vec![0u8; self.nsym];
        for &d in data {
            let fb = (d & 0x0F) ^ rem[0];
            for i in 0..self.nsym - 1 {
                rem[i] = rem[i + 1] ^ mul(fb, self.gen[i + 1]);
            }
            rem[self.nsym - 1] = mul(fb, self.gen[self.nsym]);
        }
        let mut out = Vec::with_capacity(self.n);
        out.extend(data.iter().map(|d| d & 0x0F));
        out.extend_from_slice(&rem);
        out
    }

    /// Syndromes `S_1..S_nsym` (index 0 holds `S_1`) of a wire-order word.
    fn syndromes(&self, word: &[u8]) -> Vec<u8> {
        (1..=self.nsym)
            .map(|i| {
                // word[w] is the coefficient of x^(n-1-w): Horner from the top.
                let x = alpha_pow(i as i32);
                word.iter().fold(0u8, |acc, &c| mul(acc, x) ^ c)
            })
            .collect()
    }

    /// Decode `word` (n nibbles, wire order) in place. `erasures` are wire
    /// indices whose values are unknown (their current values are ignored).
    /// Returns the number of corrected symbols (errors + erasures that changed).
    pub fn decode(&self, word: &mut [u8], erasures: &[usize]) -> Result<usize, Rs16Error> {
        let n = self.n;
        let nsym = self.nsym;
        if word.len() != n || erasures.iter().any(|&e| e >= n) {
            return Err(Rs16Error::BadParams);
        }
        if erasures.len() > nsym {
            return Err(Rs16Error::Uncorrectable);
        }
        for w in word.iter_mut() {
            *w &= 0x0F;
        }
        let synd = self.syndromes(word);
        if synd.iter().all(|&s| s == 0) {
            return Ok(0);
        }

        // Erasure locator Γ(x) = Π (1 + Y x), Y = α^(degree of erased position).
        let deg_of = |w: usize| (n - 1 - w) as i32;
        let mut gamma = vec![1u8];
        for &e in erasures {
            gamma = poly_mul(&gamma, &[1, alpha_pow(deg_of(e))]);
        }
        let rho = erasures.len();

        // Forney syndromes: T(x) = S(x) Γ(x) mod x^nsym; T_rho..T_{nsym-1}
        // obey the error locator's recurrence.
        let mut t = poly_mul(&synd, &gamma);
        t.truncate(nsym);
        let seq = &t[rho..];

        // Berlekamp–Massey on `seq` → error locator σ(x) (low-first).
        let mut c = vec![1u8];
        let mut b = vec![1u8];
        let mut l = 0usize;
        let mut m = 1usize;
        let mut bd = 1u8;
        for idx in 0..seq.len() {
            let mut d = seq[idx];
            for i in 1..=l.min(c.len() - 1).min(idx) {
                d ^= mul(c[i], seq[idx - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = mul(d, inv(bd));
            let mut next = c.clone();
            if next.len() < b.len() + m {
                next.resize(b.len() + m, 0);
            }
            for (i, &bi) in b.iter().enumerate() {
                next[i + m] ^= mul(coef, bi);
            }
            if 2 * l <= idx {
                b = c;
                l = idx + 1 - l;
                bd = d;
                m = 1;
            } else {
                m += 1;
            }
            c = next;
        }
        while c.len() > 1 && *c.last().unwrap() == 0 {
            c.pop();
        }
        let n_err = c.len() - 1;
        if n_err != l || 2 * n_err + rho > nsym {
            return Err(Rs16Error::Uncorrectable);
        }

        // Errata locator Λ = σ Γ.
        let lambda = poly_mul(&c, &gamma);
        let n_errata = lambda.len() - 1;

        // Chien search over the n valid positions (shortened code).
        let mut positions = Vec::with_capacity(n_errata);
        for w in 0..n {
            let x_inv = alpha_pow(-deg_of(w));
            if eval(&lambda, x_inv) == 0 {
                positions.push(w);
            }
        }
        if positions.len() != n_errata {
            return Err(Rs16Error::Uncorrectable);
        }

        // Forney: e = Ω(X^-1) / Λ'(X^-1)  (narrow-sense, first root α^1).
        let mut omega = poly_mul(&synd, &lambda);
        omega.truncate(nsym);
        // Formal derivative in characteristic 2: keep odd-degree terms.
        let mut dlambda = vec![0u8; lambda.len().saturating_sub(1).max(1)];
        for i in (1..lambda.len()).step_by(2) {
            dlambda[i - 1] = lambda[i];
        }
        let mut changed = 0;
        for &w in &positions {
            let x_inv = alpha_pow(-deg_of(w));
            let den = eval(&dlambda, x_inv);
            if den == 0 {
                return Err(Rs16Error::Uncorrectable);
            }
            let mag = mul(eval(&omega, x_inv), inv(den));
            if mag != 0 {
                changed += 1;
            }
            word[w] ^= mag;
        }

        if self.syndromes(word).iter().any(|&s| s != 0) {
            return Err(Rs16Error::Uncorrectable);
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            self.0 >> 8
        }
        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n
        }
    }

    #[test]
    fn field_tables_are_consistent() {
        for a in 1..16u8 {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(EXP[LOG[a as usize] as usize], a);
        }
        assert_eq!(alpha_pow(15), 1);
    }

    #[test]
    fn codewords_have_zero_syndromes() {
        let rs = Rs16::new(7, 4).unwrap();
        let cw = rs.encode(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(cw.len(), 11);
        assert_eq!(&cw[..7], &[1, 2, 3, 4, 5, 6, 7]);
        assert!(rs.syndromes(&cw).iter().all(|&s| s == 0));
    }

    /// Every combination of `e` errors and `f` erasures with `2e + f <= nsym`
    /// is corrected, for several code sizes and random data.
    #[test]
    fn corrects_errors_and_erasures_within_capacity() {
        let mut rng = Lcg(12345);
        for &(k, nsym) in &[(7usize, 2usize), (7, 4), (7, 6), (11, 4), (5, 3), (13, 2)] {
            let rs = Rs16::new(k, nsym).unwrap();
            let n = k + nsym;
            for _ in 0..400 {
                let data: Vec<u8> = (0..k).map(|_| rng.below(16) as u8).collect();
                let cw = rs.encode(&data);
                let e = rng.below(nsym / 2 + 1);
                let f = rng.below(nsym - 2 * e + 1);
                let mut pos: Vec<usize> = (0..n).collect();
                for i in 0..n {
                    let j = i + rng.below(n - i);
                    pos.swap(i, j);
                }
                let mut word = cw.clone();
                for &p in &pos[..e] {
                    word[p] ^= 1 + rng.below(15) as u8;
                }
                let erasures: Vec<usize> = pos[e..e + f].to_vec();
                for &p in &erasures {
                    word[p] = rng.below(16) as u8;
                }
                let res = rs.decode(&mut word, &erasures);
                assert!(res.is_ok(), "k={k} nsym={nsym} e={e} f={f}: {res:?}");
                assert_eq!(word, cw, "k={k} nsym={nsym} e={e} f={f}");
            }
        }
    }

    #[test]
    fn too_many_errors_rarely_miscorrect() {
        // Beyond capacity the decoder must mostly report failure; any "success"
        // must at least be a valid codeword (syndromes re-checked).
        let rs = Rs16::new(7, 4).unwrap();
        let mut rng = Lcg(999);
        let mut miscorrected = 0;
        for _ in 0..2000 {
            let data: Vec<u8> = (0..7).map(|_| rng.below(16) as u8).collect();
            let cw = rs.encode(&data);
            let mut word = cw.clone();
            for p in [0usize, 3, 6, 9] {
                word[p] ^= 1 + rng.below(15) as u8;
            }
            if rs.decode(&mut word, &[]).is_ok() {
                assert!(rs.syndromes(&word).iter().all(|&s| s == 0));
                assert_ne!(word, cw);
                miscorrected += 1;
            }
        }
        // Shortened RS(11,7): a random word lands within distance 2 of some
        // codeword with probability roughly 16^7 * (1 + 11*15 + 55*225) / 16^11.
        assert!(miscorrected < 600, "miscorrected {miscorrected}/2000");
    }
}
