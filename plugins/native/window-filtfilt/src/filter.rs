//! Butterworth 高通设计 + 零相位滤波（scipy butter/filtfilt 的等价实现）。
//!
//! filtfilt 复刻 scipy 路径：奇延拓 padlen（MATLAB 约定 3·(nfilt−1)，
//! 由调用方传入）+ lfilter_zi 初始条件 + 正反两遍 DF2T 滤波。

use num_complex::Complex64;

/// 复根多项式展开（np.poly）：首一多项式系数，降幂。
fn poly(roots: &[Complex64]) -> Vec<Complex64> {
    let mut c = vec![Complex64::new(1.0, 0.0)];
    for r in roots {
        let mut nc = vec![Complex64::new(0.0, 0.0); c.len() + 1];
        for (i, ci) in c.iter().enumerate() {
            nc[i] += *ci;
            nc[i + 1] -= *ci * *r;
        }
        c = nc;
    }
    c
}

/// Butterworth 滤波类型（bandpass/bandstop 需要双截止，另立参数后再扩）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BType {
    Highpass,
    Lowpass,
}

/// scipy.signal.butter(order, wn, btype)：wn 为归一化截止（fc / (fs/2)）。
/// 返回 (b, a)，a[0] = 1。highpass 路径与 scipy 逐位对齐（金标准夹具）；
/// lowpass 走同一套 zpk 机制（lp2lp + bilinear），以设计性质测试守护。
pub fn butter(order: usize, wn: f64, btype: BType) -> (Vec<f64>, Vec<f64>) {
    use std::f64::consts::PI;
    let nf = order as f64;
    // 模拟原型极点（scipy buttap）：m = -N+1, -N+3, …, N-1；p = -exp(j·π·m/(2N))
    let proto: Vec<Complex64> = (0..order)
        .map(|i| {
            let m = (-(order as i64) + 1 + 2 * i as i64) as f64;
            -(Complex64::new(0.0, PI * m / (2.0 * nf))).exp()
        })
        .collect();
    // 预畸变（scipy iirfilter 内部 fs=2.0）
    let fs = 2.0;
    let warped = 2.0 * fs * (PI * wn / fs).tan();
    let (z_a, p_a, k_a): (Vec<Complex64>, Vec<Complex64>, f64) = match btype {
        BType::Highpass => {
            // lp2hp_zpk：零点集为空 → k_hp = real(1/prod(-p))；极点 p → warped/p；
            // 附加 order 个原点零点
            let prod_neg_p: Complex64 = proto.iter().map(|p| -*p).product();
            let k_hp = (Complex64::new(1.0, 0.0) / prod_neg_p).re;
            let p_hp: Vec<Complex64> = proto.iter().map(|p| Complex64::new(warped, 0.0) / *p).collect();
            (vec![Complex64::new(0.0, 0.0); order], p_hp, k_hp)
        }
        BType::Lowpass => {
            // lp2lp_zpk：p → warped·p；k *= warped^degree（degree = order，零点集为空）
            let p_lp: Vec<Complex64> = proto.iter().map(|p| Complex64::new(warped, 0.0) * *p).collect();
            (Vec::new(), p_lp, warped.powi(order as i32))
        }
    };
    // bilinear_zpk（fs=2 → fs2=4）；模拟零点少于极点的差额补 z=-1 零点
    let fs2 = 2.0 * fs;
    let mut z_d: Vec<Complex64> = z_a.iter().map(|z| (Complex64::new(fs2, 0.0) + z) / (Complex64::new(fs2, 0.0) - z)).collect();
    let p_d: Vec<Complex64> = p_a.iter().map(|p| (Complex64::new(fs2, 0.0) + p) / (Complex64::new(fs2, 0.0) - p)).collect();
    let num: Complex64 = z_a.iter().map(|z| Complex64::new(fs2, 0.0) - z).product();
    let den: Complex64 = p_a.iter().map(|p| Complex64::new(fs2, 0.0) - p).product();
    let k_d = k_a * (num / den).re;
    z_d.resize(p_d.len(), Complex64::new(-1.0, 0.0));
    let b: Vec<f64> = poly(&z_d).iter().map(|c| (k_d * c).re).collect();
    let a: Vec<f64> = poly(&p_d).iter().map(|c| c.re).collect();
    (b, a)
}

/// 小规模高斯消元（部分主元）。
fn solve(mut m: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Vec<f64> {
    let n = rhs.len();
    for col in 0..n {
        let piv = (col..n).max_by(|&a, &b| m[a][col].abs().partial_cmp(&m[b][col].abs()).unwrap()).unwrap();
        m.swap(col, piv);
        rhs.swap(col, piv);
        let d = m[col][col];
        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = m[row][col] / d;
            for k in col..n {
                m[row][k] -= factor * m[col][k];
            }
            rhs[row] -= factor * rhs[col];
        }
    }
    (0..n).map(|i| rhs[i] / m[i][i]).collect()
}

/// scipy.signal.lfilter_zi：单位阶跃稳态初始条件。
pub fn lfilter_zi(b: &[f64], a: &[f64]) -> Vec<f64> {
    let n = a.len().max(b.len());
    let an: Vec<f64> = (0..n).map(|i| a.get(i).copied().unwrap_or(0.0) / a[0]).collect();
    let bn: Vec<f64> = (0..n).map(|i| b.get(i).copied().unwrap_or(0.0) / a[0]).collect();
    let m = n - 1;
    // M = I − companion(a)ᵀ
    let mut mat = vec![vec![0.0; m]; m];
    for i in 0..m {
        mat[i][i] = 1.0;
    }
    for j in 0..m {
        mat[j][0] += an[j + 1];
    }
    for i in 1..m {
        mat[i - 1][i] -= 1.0;
    }
    let rhs: Vec<f64> = (0..m).map(|i| bn[i + 1] - an[i + 1] * bn[0]).collect();
    solve(mat, rhs)
}

/// Direct Form II Transposed 单向滤波（scipy.signal.lfilter，含初始状态）。
pub fn lfilter(b: &[f64], a: &[f64], x: &[f64], zi: &[f64]) -> Vec<f64> {
    let n = a.len().max(b.len());
    let bn: Vec<f64> = (0..n).map(|i| b.get(i).copied().unwrap_or(0.0) / a[0]).collect();
    let an: Vec<f64> = (0..n).map(|i| a.get(i).copied().unwrap_or(0.0) / a[0]).collect();
    let m = n - 1;
    let mut z = zi.to_vec();
    debug_assert_eq!(z.len(), m);
    let mut y = Vec::with_capacity(x.len());
    for &xv in x {
        let yv = bn[0] * xv + z[0];
        for k in 0..m - 1 {
            z[k] = z[k + 1] + bn[k + 1] * xv - an[k + 1] * yv;
        }
        z[m - 1] = bn[m] * xv - an[m] * yv;
        y.push(yv);
    }
    y
}

/// 零相位滤波：scipy.signal.filtfilt(padtype='odd', padlen=padlen)。
/// MATLAB filtfilt 等价于 padlen = 3·(max(len(a),len(b))−1)。
pub fn filtfilt(b: &[f64], a: &[f64], x: &[f64], padlen: usize) -> Result<Vec<f64>, String> {
    let e = padlen;
    if x.len() <= e {
        return Err(format!("filtfilt: input length {} <= padlen {}", x.len(), e));
    }
    // 奇延拓
    let mut ext = Vec::with_capacity(x.len() + 2 * e);
    let x0 = x[0];
    for i in (1..=e).rev() {
        ext.push(2.0 * x0 - x[i]);
    }
    ext.extend_from_slice(x);
    let xl = x[x.len() - 1];
    for i in 2..=e + 1 {
        ext.push(2.0 * xl - x[x.len() - i]);
    }

    let zi = lfilter_zi(b, a);
    let zi_f: Vec<f64> = zi.iter().map(|v| v * ext[0]).collect();
    let y = lfilter(b, a, &ext, &zi_f);
    let mut rev: Vec<f64> = y.into_iter().rev().collect();
    let zi_b: Vec<f64> = zi.iter().map(|v| v * rev[0]).collect();
    rev = lfilter(b, a, &rev, &zi_b);
    rev.reverse();
    Ok(rev[e..e + x.len()].to_vec())
}
