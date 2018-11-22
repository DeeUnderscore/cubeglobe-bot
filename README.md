# cubeglobe-bot

This is a Fediverse bot which uses the [cubeglobe](https://github.com/DeeUnderscore/cubeglobe) library to generate images and post them via the Mastodon API.


## How to build
### Normal mode
In normal mode, you will need SDL2 installed on your system. Check out [rust-sdl2's readme](https://github.com/Rust-SDL2/rust-sdl2/blob/master/README.md) for more details on this. With SDL2 installed, you can build with Cargo:

```shell
cargo build --release
```

### Bundled mode
To use rust-sdl2 bundled mode, enable feature `sdlbundled`:

```shell
cargo build --release --features sdlbundled
```

## How to run
1. Copy `example.config.toml` to `config.toml`.
2. Fill out `config.toml` with the relevant credentials. This program does not register as an app or obtain a token, you will have to do it yourself.
3. Take a look at `cubeglobe/assets/full-tiles.toml`. It contains the path to the assets directory. You may wish to copy this file and edit the path so it reflects the situation on your system and points to where the assets directory is.
4. Run with `cubeglobe-bot --tiles path/to/your/full-tiles.toml`