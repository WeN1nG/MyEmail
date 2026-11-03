# assets

## fonts/cjk-subset.ttf

仓库内已提供 Noto Sans SC（OFL 1.1）子集：GB2312 一、二级汉字 + 全角标点 + 拉丁/希腊/西里尔字母，
约 2.2 MB。`me-app/build.rs` 在构建时检测并通过 `include_bytes!` 内嵌进二进制；无系统 CJK 字体时也能正常显示。

重新生成（需要 `fonttools`，源字体为 Windows 自带的 Noto Sans SC 可变字体）：

```bash
python -m pip install fonttools
python -I scripts/build-cjk-subset.py
```

许可证全文见 `assets/fonts/OFL.txt`。文件不存在时，应用退化为运行时加载系统字体：

- Windows：`msyh.ttc` / `simhei.ttf` / `simsun.ttc`
- macOS：`PingFang.ttc` 等
- Linux：Noto Sans CJK / 文泉驿

系统字体也不可用时界面中的中文会显示为方框，日志会给出 WARN。

## icons/app.ico

窗口与安装包图标。当前未在代码中引用；打包阶段接入。