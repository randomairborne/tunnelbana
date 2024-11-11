# Tunnelbana

Tunnelbana is a collection of crates designed to make fast Rust static file servers with
[tower](https://github.com/tower-rs/tower). It's named for the swedish word for subway.

[tunnelbana-etags](https://crates.io/crates/tunnelbana-etags)
[tunnelbana-headers](https://crates.io/crates/tunnelbana-headers)
[tunnelbana-hidepaths](https://crates.io/crates/tunnelbana-hidepaths)
[tunnelbana-redirects](https://crates.io/crates/tunnelbana-redirects)

## Why?

Tunnelbana was created to reduce my dependence on Cloudflare. I love Cloudflare and their services,
but I don't love the direction they're going with the pages product, so I thought I'd make my own
thing.

## How do I use it?

Tunnelbana is available on [crates.io](https://crates.io/crates/tunnelbana), where it can be easily
used as a binary with `tunnelbana /var/www/html`.
It is also available on [ghcr.io](https://github.com/randomairborne/tunnelbana/pkgs/container/tunnelbana),
where it can be used with bind mounts or as a build source for a container as demonstrated below.

## Docker

```dockerfile
FROM node:alpine AS builder

# You could also build your app any other way, as long as it outputs a directory that can be copied
# to some path in a container.
COPY ./xpd-web/ /build

WORKDIR /build/

RUN npm install

RUN npm run build

# asset squisher is a seperate project, made by the same author. It works well with tower-http
# precompression, which tunnelbana uses.
FROM ghcr.io/randomairborne/asset-squisher:latest AS compressor

COPY --from=builder /build/dist/ /build/dist/

RUN asset-squisher --no-compress-images /build/dist/ /build/compressed/

FROM ghcr.io/randomairborne/tunnelbana:latest

COPY --from=compressor /build/compressed/ /var/www/html/

CMD ["tunnelbana", "/var/www/html"]
```

## Configuration

### Headers

Headers can be customized with the `/_headers` file in the root of the directory.
Headers syntax is an unindented target path, followed by a list of indented `key: value` pairs.
You can use `{named_captures}` in the target path, and at the end you can use `{*wildcards}`.

Limitations:

- It is not possible to have a wildcard affect pages with more specific headers. Create an issue if you need this.

```plaintext
/my/cool/header
    X-Cool-Header: radical
/{lol}/header_path
    X-Lol: very funny.
/{*everything}
    X-On-Every-Page: on_every_page
```

### Redirects

Redirects can be customized with the `/_redirects` file in the root of the directory.
Redirect syntax is very simple. There are three space-seperated columns on each line of text:
the path where the redirect will apply, the target (with interpolations), and an optional status code.
You can use the same `{capturing_item}` and `{*wildcards}` at the ends
as in the headers, and they can even be used in the target with `{capturing_name}`.

Limitations:

- You cannot have a wildcard with a suffix, it must be a suffix for the redirect.

```plaintext
/boring https://example.org 302
/{capture}/ /en/{capture}/
/en/{*splat} /{splat}
```
