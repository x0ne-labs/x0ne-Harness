use std::collections::HashMap;

pub struct Config {
    pub values: HashMap<String, String>,
}

impl Config {
    pub fn load(path: &str) -> Config {
        let text = std::fs::read_to_string(path).unwrap();
        let mut values = HashMap::new();
        for line in text.lines() {
            let (k, v) = line.split_once('=').unwrap();
            values.insert(k.trim().to_string(), v.trim().to_string());
        }
        Config { values }
    }

    // TODO: typed getters
    pub fn get(&self, key: &str) -> &str {
        self.values.get(key).unwrap()
    }
}
