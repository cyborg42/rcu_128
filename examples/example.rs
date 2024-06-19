use std::{
    thread::sleep,
    time::{Duration, Instant},
};
fn main() {
    let x = rcu_128::RcuCell::new("0".to_string());
    std::thread::scope(|s| {
        s.spawn(|| {
            for i in 0..40 {
                let t = Instant::now();
                x.update(i.to_string());
                println!("Update used time: {:?}", t.elapsed());
                sleep((t + Duration::from_millis(100)).duration_since(Instant::now()));
            }
        });
        s.spawn(|| {
            // Always has 4 guards alive
            let mut guards = [x.read(), x.read(), x.read(), x.read()];
            for idx in 0..400 {
                let r = x.read();
                println!("Read value: {}", *r);
                guards[idx % 4] = r;
                sleep(Duration::from_millis(10));
            }
        });
    })
}
