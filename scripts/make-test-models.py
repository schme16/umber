# Writes two OBJ test models into the directory given: knot-no-uvs.obj (a torus knot with no
# UVs, for Auto-unwrap) and crate-loose-uvs.obj (a box whose faces are bunched into one corner
# of the UV square, for Repack). Used by scripts/captures/unwrap.sh and repack.sh.
#   python3 scripts/make-test-models.py target/models
import math, sys
out = sys.argv[1]
import os; os.makedirs(out, exist_ok=True)
# Torus knot, no UVs.
P, Q, N, M, R, r = 2, 3, 240, 24, 1.0, 0.28
def knot(t):
    rr = 0.5 * (2 + math.cos(Q * t))
    return (rr * math.cos(P * t), rr * math.sin(P * t), -0.5 * math.sin(Q * t))
def sub(a,b): return tuple(x-y for x,y in zip(a,b))
def cross(a,b): return (a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0])
def norm(a):
    l = math.sqrt(sum(x*x for x in a)); return tuple(x/l for x in a)
with open(f"{out}/knot-no-uvs.obj", "w") as f:
    for i in range(N):
        t = 2*math.pi*i/N
        c = knot(t); d = norm(sub(knot(t+1e-3), c))
        n = norm(cross(d, (0,0,1))); b = cross(d, n)
        for j in range(M):
            a = 2*math.pi*j/M
            p = tuple(c[k] + r*(math.cos(a)*n[k] + math.sin(a)*b[k]) for k in range(3))
            f.write("v %f %f %f\n" % p)
    for i in range(N):
        for j in range(M):
            a = i*M + j + 1; b2 = ((i+1)%N)*M + j + 1
            c2 = ((i+1)%N)*M + (j+1)%M + 1; d2 = i*M + (j+1)%M + 1
            f.write(f"f {a} {b2} {c2}\nf {a} {c2} {d2}\n")
# Crate: a box whose six faces are packed wastefully into the bottom-left corner.
with open(f"{out}/crate-loose-uvs.obj", "w") as f:
    faces = [((1,0,0),(0,0,-1),(0,1,0)),((-1,0,0),(0,0,1),(0,1,0)),((0,1,0),(1,0,0),(0,0,-1)),
             ((0,-1,0),(1,0,0),(0,0,1)),((0,0,1),(1,0,0),(0,1,0)),((0,0,-1),(-1,0,0),(0,1,0))]
    vi = 1
    for k,(n,u,v) in enumerate(faces):
        cell = ((k%3)*0.14 + 0.02, (k//3)*0.14 + 0.02)
        for su,sv in [(-1,-1),(1,-1),(1,1),(-1,1)]:
            p = tuple(n[i] + u[i]*su + v[i]*sv for i in range(3))
            f.write("v %f %f %f\n" % p)
            f.write("vt %f %f\n" % (cell[0] + (su+1)*0.06, cell[1] + (sv+1)*0.06))
        f.write(f"f {vi}/{vi} {vi+1}/{vi+1} {vi+2}/{vi+2}\nf {vi}/{vi} {vi+2}/{vi+2} {vi+3}/{vi+3}\n")
        vi += 4
