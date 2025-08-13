use jambox_go_rs::rewriter;

#[test]
fn rewrite_playlist_urls_basic() {
    // a small HLS playlist with comments and media entries
    let input =
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4,\nseg_0.ts\n#EXTINF:4,\nseg_1.ts\n#EXT-X-ENDLIST\n";
    let out = rewriter::rewrite_playlist_urls(input, 7);
    let expected = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4,\n/channel/7/seg_0.ts\n#EXTINF:4,\n/channel/7/seg_1.ts\n#EXT-X-ENDLIST\n";
    assert_eq!(out, expected);
}

#[test]
fn rewrite_upstream_playlist_segments_key_line() {
    // ensure we can rewrite a key URI to absolute
    let input =
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.key\",IV=0xabcdef\n#EXTINF:4,\nseg_0.ts\n";
    let base = "https://example.com/path/playlist.m3u8";
    let out = rewriter::rewrite_upstream_playlist_segments(input, base);
    assert!(out.contains("URI=\"https://example.com/path/key.key\""));
}
