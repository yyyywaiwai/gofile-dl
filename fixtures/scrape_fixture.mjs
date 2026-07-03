process.stdout.write(
  JSON.stringify({
    token: "test-token",
    root: {
      name: "root",
      type: "folder",
      children: {
        "folder-id": {
          name: "sub/folder",
          type: "folder",
          children: {
            "file-id": {
              name: "video.mp4",
              type: "file",
              size: 42,
              link: "https://example.com/video.mp4",
            },
          },
        },
      },
    },
  }),
);
