fn main() {
    use std::thread::sleep;
    let x = rcu_128::RcuCell::new("0".to_string());
    std::thread::scope(|s| {
        s.spawn(|| {
            for i in 0..40 {
                sleep(std::time::Duration::from_millis(100));
                let t = std::time::Instant::now();
                x.write(i.to_string());
                println!("{:?}", t.elapsed());
            }
        });
        s.spawn(|| {
            let mut guards: [rcu_128::RcuGuard<String>; 4] =
                [x.read(), x.read(), x.read(), x.read()];
            for idx in 0..400 {
                let r = x.read();
                println!("{}", *r);
                guards[idx % 4] = r;
                sleep(std::time::Duration::from_millis(10));
            }
        });
    })
}
