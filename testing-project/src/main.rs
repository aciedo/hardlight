use hardlight::{
    factory, ApplicationClient, Compression, Server, ServerConfig,
    ServerHandler,
};
use indicatif::{ProgressBar, ProgressStyle};
use plotters::{prelude::*, style::full_palette::PINK_200};
use std::{sync::Arc, collections::HashMap};
use tokio::{
    runtime::Builder,
    sync::mpsc,
    time::{sleep, timeout, Duration, Instant},
};
use tracing::info;

mod handler;
mod service;

use handler::Handler;
use service::{Counter, CounterClient};

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();

    let config = ServerConfig::new_self_signed("localhost:8080");
    info!("{:?}", config);
    let server = Server::new(config, factory!(Handler));
    tokio::spawn(async move { server.run().await.unwrap() });

    // wait for the server to start
    sleep(Duration::from_millis(10)).await;

    let num_clients = 10;
    let tasks_per_client = 3;
    let invocs_per_task = 50_000;
    let compression = Compression::none();
    info!(
        "Running {} clients, {} tasks per client, {} invocations per task\n",
        num_clients, tasks_per_client, invocs_per_task
    );

    let (send, mut recv) = mpsc::unbounded_channel();

    for _ in 0..num_clients {
        let sender = send.clone();
        tokio::spawn(async move {
            let mut client =
                CounterClient::new_self_signed("localhost:8080", compression);
            client.connect().await.unwrap();
            let client = Arc::new(client);
            let mut tasks = Vec::new();
            for _ in 0..tasks_per_client {
                let client = client.clone();
                let sender = sender.clone();
                tasks.push(tokio::spawn(async move {
                    for _ in 0..invocs_per_task {
                        let start = Instant::now();
                        let _ = client.test_overhead().await;
                        let _ = sender.send(start.elapsed());
                    }
                }));
            }
            for task in tasks {
                task.await.expect("task failed");
            }
        });
    }

    let mut timings: Vec<Duration> = Vec::new();

    let bar = ProgressBar::new(num_clients as u64 * tasks_per_client as u64 * invocs_per_task as u64).with_style(ProgressStyle::default_bar().template("{spinner:.blue} [{elapsed_precise}] ({eta}) {bar:50.green/blue} {pos:>7}/{len:7} {per_sec} {msg}").unwrap().progress_chars("█░⎯"));

    sleep(Duration::from_secs(1)).await;
    loop {
        match timeout(Duration::from_millis(10), recv.recv())
            .await
            .ok()
            .flatten()
        {
            Some(elapsed) => {
                timings.push(elapsed);
                bar.inc(1);
            }
            None => break,
        };
    }

    bar.finish_with_message("done\n");

    plot_scatter_graph(&timings);
    plot_histogram_graph(&timings);

    timings.sort();
    let sum: u128 = timings.iter().map(|t| t.as_micros()).sum();
    let avg = sum / timings.len() as u128;
    let min = timings.first().unwrap().as_micros();
    let max = timings.last().unwrap().as_micros();
    let med = timings[timings.len() / 2].as_micros();
    let std_dev = timings
        .iter()
        .map(|t| (t.as_micros() as u128 - avg as u128).pow(2) as u128)
        .sum::<u128>()
        .checked_div(timings.len() as u128)
        .map(|v| v as f64)
        .map(|v| v.sqrt())
        .unwrap_or(0.0)
        .round() as u128;
    
    // count how many timings are between the mean and mean-std_dev
    let mut count = 0;
    for timing in timings.iter() {
        if timing.as_micros() < avg - std_dev {
            count += 1;
        }
    }
    let percentage = count as f64 / timings.len() as f64 * 100.0;
    info!("{}% ({}) of timings are below the mean-std_dev", percentage, count);
    
    // and the same for mean and mean+std_dev
    let mut count = 0;
    for timing in timings.iter() {
        if timing.as_micros() > avg + std_dev {
            count += 1;
        }
    }
    let percentage = count as f64 / timings.len() as f64 * 100.0;
    info!("{}% ({}) of timings are above the mean+std_dev", percentage, count);

    plot_percentile_graph(&timings);

    info!("(µs) med:{med}; avg:{avg}; std_dev:{std_dev}; min:{min}; max:{max}");

    Ok(())
}

fn plot_percentile_graph(timings: &Vec<Duration>) {
    let mut data = Vec::new();
    for i in 0..100 {
        let percentile = timings[(timings.len() * i) / 100].as_micros();
        data.push((i as u128, percentile));
    }
    let root =
        BitMapBackend::new("percentile.png", (3840, 2160)).into_drawing_area();
    root.fill(&WHITE).unwrap();
    let mut chart = ChartBuilder::on(&root)
        .caption("Percentile", ("sans-serif", 50).into_font())
        .margin(15)
        .x_label_area_size(30)
        .y_label_area_size(30)
        // use 99.99th percentile as max
        .build_cartesian_2d(0u128..100u128, 0u128..1000u128)
        .unwrap();
    chart
        .configure_mesh()
        .disable_x_mesh()
        .disable_y_mesh()
        .y_desc("Time (us)")
        .x_desc("Percentile")
        .axis_desc_style(("sans-serif", 15).into_font())
        .draw()
        .unwrap();
    chart
        .draw_series(LineSeries::new(data, &RED))
        .unwrap()
        .label("Percentile")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));
    chart
        .configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()
        .unwrap();
    root.present().unwrap();
    info!("Percentile graph generated at ./percentile.svg")
}

fn plot_scatter_graph(timings: &Vec<Duration>) {
    // timings isn't sorted
    let root = BitMapBackend::new("scatter.png", (3840, 2160)).into_drawing_area();
    root.fill(&WHITE).unwrap();
    let mut chart = ChartBuilder::on(&root)
        .caption("Scatter", ("sans-serif", 50).into_font())
        .margin(15)
        .x_label_area_size(30)
        .y_label_area_size(30)
        // use largest value + 10 as upper bound for y axis
        .build_cartesian_2d(
            0u128..timings.len() as u128,
            0u128..timings.iter().max().unwrap().as_micros(),
        )
        .unwrap();
    chart
        .configure_mesh()
        .disable_x_mesh()
        .disable_y_mesh()
        .y_desc("Time (us)")
        .x_desc("Invocation")
        .axis_desc_style(("sans-serif", 15).into_font())
        .draw()
        .unwrap();
    let mut data = Vec::new();
    for (i, timing) in timings.iter().enumerate() {
        data.push((i as u128, timing.as_micros()));
    }
    chart
        .draw_series(PointSeries::of_element(data, 1, ShapeStyle::from(&RED).filled(), &|c, s, st| {
            return EmptyElement::at(c) + Circle::new((0, 0), s, st.filled());
        }))
        .unwrap()
        .label("Scatter")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));

    // draw average line
    let sum: u128 = timings.iter().map(|t| t.as_micros()).sum();
    let avg = sum / timings.len() as u128;
    let avg_line = [(0, avg), (timings.len() as u128, avg)];
    chart
        .draw_series(LineSeries::new(avg_line, &BLUE))
        .unwrap()
        .label("Average")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLUE));

    // add standard deviation (+/-)
    let std_dev = standard_deviation(timings);
    let std_dev_line = [
        (0, avg - std_dev as u128),
        (timings.len() as u128, avg - std_dev as u128),
    ];
    chart
        .draw_series(LineSeries::new(std_dev_line, &BLACK))
        .unwrap()
        .label("Standard Deviation")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLACK));
    let std_dev_line = [
        (0, avg + std_dev as u128),
        (timings.len() as u128, avg + std_dev as u128),
    ];
    chart
        .draw_series(LineSeries::new(std_dev_line, &BLACK))
        .unwrap();

    chart
        .configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()
        .unwrap();
    root.present().unwrap();
    info!("Scatter graph generated at ./scatter.svg")
}

fn plot_histogram_graph(timings: &Vec<Duration>) {
    // Create the histogram data
    let mut histogram: HashMap<u128, u128> = HashMap::new();
    for timing in timings {
        let bin = timing.as_micros();
        *histogram.entry(bin).or_insert(0) += 1;
    }

    // Convert HashMap to Vec for plotting
    let mut data: Vec<(u128, u128)> = histogram.into_iter().collect();
    
    // Sort data by time
    data.sort_by(|a, b| a.0.cmp(&b.0));

    let root = BitMapBackend::new("histogram.png", (3840, 2160)).into_drawing_area();
    root.fill(&WHITE).unwrap();
    
    // Use the largest count as max y value, and largest bin (time) * 5 as max x value
    let max_y_value = data.iter().map(|(_, count)| *count).max().unwrap_or_default();
    let max_x_value = 1500;
    
    // Create the chart builder
    let mut chart = ChartBuilder::on(&root)
        .margin(10)
        .x_label_area_size(30)
        .y_label_area_size(30)
        .build_cartesian_2d(
            0u32..(max_x_value as u32),
            0u32..(max_y_value as u32),
        )
        .unwrap();
    
    chart.configure_mesh().y_desc("Number of Invocations").x_desc("Time (us)").axis_desc_style(("sans-serif", 15).into_font()).draw().unwrap();
    
    // Define a histogram series with data
    let histogram = Histogram::vertical(&chart)
        .style(PINK_200.filled())
        .data(data.iter().map(|(x, y)| ((*x) as u32, *y as u32)));
    
    // Draw the histogram series
    chart.draw_series(histogram).unwrap();

    // Save the chart
    root.present().unwrap();

    info!("Histogram graph generated at ./histogram.png")
}

fn standard_deviation(timings: &Vec<Duration>) -> f64 {
    let sum: u128 = timings.iter().map(|t| t.as_micros()).sum();
    let avg = sum / timings.len() as u128;
    let mut variance = 0.0;
    for timing in timings {
        let diff = timing.as_micros() as f64 - avg as f64;
        variance += diff * diff;
    }
    (variance / timings.len() as f64).sqrt()
}
