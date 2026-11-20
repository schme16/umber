import sys, re, pathlib
src = pathlib.Path(sys.argv[1]).read_text()
out = pathlib.Path(sys.argv[2])
keys = ["#0f1012","#131417","#24262a","#1a1b1e","#1d1e22","#2e3035","#2a2b30","#2a2b31","#1b1c20","#2a2c31","#2d2f35","#34363c",
        "#ecebe8","#c9cad0","#9a9ca3","#a3a5ab","#f3f2ef","#b4b6bc","#f0a646","#ffc27a","#1a1206","#2f2a22","#7a5a2c","#f6c886",
        "#4a3d28","#121316","#4a4c52"]
themes = {
 "1-current-graphite": keys,
 "2-umber-brass": ["#110f0d","#15120f","#2a251f","#1b1b1c","#1d1915","#332c25","#2a241e","#2a241e","#17140f","#2e2822","#352d25","#3a332b",
        "#f1e9dc","#cdbfac","#a39582","#a89a87","#f6efe3","#bcae9a","#c9a063","#ddb67c","#1b1309","#2e261b","#7d6440","#e6c896",
        "#5a4a33","#120f0c","#544a3f"],
 "3-burnt-umber-terracotta": ["#120d0b","#16100d","#2c211c","#1b1b1c","#1f1714","#382a23","#2d221d","#2d221d","#1a1310","#31251f","#3a2c25","#40322a",
        "#f3e8df","#d2c0b2","#a99484","#ad9988","#f7ede5","#c2ad9d","#d0794a","#e28f62","#1c0c05","#33201a","#8a4a2c","#f0b08c",
        "#5e3a28","#130e0c","#5a473d"],
 "4-parchment-light": ["#ece6dd","#f6f2ec","#ddd5ca","#5b5a58","#faf7f2","#e0d8cc","#efe8de","#efe8de","#efe9e0","#ddd5ca","#ffffff","#ddd4c7",
        "#231c17","#4a3f36","#6e6155","#6e6155","#231c17","#5a4e44","#6b4a33","#7f5a40","#fbf6ef","#efe3d6","#b89878","#5a3c27",
        "#c9ab8c","#e6ded3","#c9bfb2"],
}
for name, vals in themes.items():
    s = src.replace(">UV Paint<", ">Umber<")
    m = dict(zip(keys, vals))
    s = re.sub("|".join(re.escape(k) for k in keys), lambda mo: m[mo.group(0).lower()], s, flags=re.I)
    if name.startswith("4"):
        s = s.replace("rgba(0,0,0,.45)", "rgba(60,40,20,.12)").replace("rgba(0,0,0,.5)", "rgba(60,40,20,.25)")
        s = s.replace("background:#231c17;box-shadow", "background:#ffffff;border:1px solid #c9bfb2;box-shadow")
    (out / f"{name}.dc.html").write_text(s)
