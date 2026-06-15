Tatsuma Wada
Keio University

1. How can we generate uniform random numbers?

   (a) Set parameters $a = 1229$, $c = 351750$, $m = 1664501$, and $I_0 = 1990103011$.
   (b) Compute the remainder of $(aI_0 + c)/m$ (use, for example, a MATLAB function "mod(a*I0+c,m)")
   (c) Compute the $x_0 = \text{remainder}/m$
   (d) Set the remainder to $I_1$.
   (e) Iterate (b) to (d) 1,000 times, i.e., obtain $x_t$ and $I_t$ for $t = 1, \dots, T$ and $T = 1000$.
   (f) Make a histogram for $x$.

2. How can we generate normal random numbers from uniform random numbers?

   (a) Generate two uniform random variables $U_1$ and $U_2$.
   (b) Compute

$$
X_1 = \sqrt{-2 \log U_1} \cos(2\pi U_2)
$$

```math
X_1 = \sqrt{-2 \log U_1} \cos(2\pi U_2)
```


```rs
impl Default for HistogramApp {
    fn default() -> Self {
        // Replicating your original seed: 511
        // let mut rng = // rand::rngs::StdRng::seed_from_u64(511);
        let mut rng = UniformRng{
            a: 1229,
            c: 351750,
            m: 1664501,
            I: 1990103011,
        };

        let normal = Normal::new(0.0, 1.0).unwrap();

        
        let bin_width: f64 = 0.05; // Explicitly f64
        let mut bins = std::collections::BTreeMap::new();

        let T: usize = 1000;
        for _ in 0..T{
            // println!("{}", rng.next());
    
            let v: f64 = rng.next_normal();//normal.sample(&mut rng);
            // Explicitly hint that the result of the division is f64
            let bin_idx = (v / bin_width).floor() as i32;
            *bins.entry(bin_idx).or_insert(0) += 1;
        }

        let bars = bins
            .into_iter()
            .map(|(idx, count)| {
                let x = idx as f64 * bin_width + (bin_width / 2.0);
                Bar::new(x, count as f64).width(bin_width)
            })
            .collect();

        Self { bars }
    }
}
```
