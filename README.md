# Typemusic - music as you type
Typemusic takes in an wav file to your music path and then plays the music each time you type and loops it over again. It is built in Rust using Cpal, hound and rdev. 
Just run the file and it will ask for the wav file path and then wait till it says typemusic is running, then you can start typing and enjoy the music. 

# Download:
You can download it for windows amd64 using the releases page. For other OS and architecture you need to  build it yourself like this:
```bash
cargo build --release
```
The output will be in the target/release folder.