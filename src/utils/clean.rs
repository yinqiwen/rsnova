pub async fn clean_rotate_logs(path: String) {
    let format =
        time::format_description::parse("[year]-[month]-[day]").expect("Unable to create format");
    // let format = time::format_description::parse("[year]-[month]-[day]-[hour]-[minute]")
    //     .expect("Unable to create format");

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        let now = time::OffsetDateTime::now_utc();
        for i in 7..=30 {
            let check_date = now.clone();
            match check_date.checked_sub(time::Duration::days(i)) {
                Some(date) => {
                    let suffix = date
                        .format(&format)
                        .expect("Unable to format OffsetDateTime");
                    let file_path = format!("{}.{}", path, suffix);
                    let path = std::path::Path::new(file_path.as_str());
                    if path.exists() {
                        tracing::info!("remove log file:{}", file_path);
                        let _ = std::fs::remove_file(file_path);
                    } else {
                        //tracing::info!("remove log file:{} not exist", file_path);
                        break;
                    }
                }
                None => {
                    break;
                }
            }
        }
    }
}
