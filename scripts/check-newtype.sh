#!/usr/bin/env bash
# plan/00 §4.3 第 3 条与 eng/03 §2.1 的执行器：两个池 newtype 不得实现 Deref，
# 也不得开出 Deref 之外的等价逃生口。
#
# 为什么是 shell 而不是一条 Rust 测试：这两条判据的命题是「某个 impl 不存在」，
# 而运行时测试看不见不存在的东西。试过两条 Rust 路子，都失败且**都表现为通过**：
#
#   1 用 `impl<T: ?Sized> NotDeref for T {}` 这种全覆盖 blanket impl 当断言 ——
#     它对任何类型都成立，跟 Deref 在不在毫无关系，纯粹是空转的绿。
#   2 用 autoref 特化技巧（特化侧挂 Probe<T>、兜底侧挂 &Probe<T>）——
#     在泛型函数里做不到：bound 不满足时 rustc 直接报 E0599，
#     不会继续尝试自动取引用。实测两个方向都编译失败。
#     第一版把两侧写反了，探针永远返回 false，加上 Deref 后测试照样全绿 ——
#     那是「验证手段坏掉时表现为通过」的一例，靠反证才发现。
#
# 所以改成在源码层面查。它的局限写在末尾。
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
check() {   # $1=文件 $2=类型名 $3=判据出处 $4=获批逃生口的正则（空=一个都不许有）
  local file="$1" ty="$2" src="$3" allow="${4-}"

  if ! [ -f "$file" ]; then
    echo "✗ $file 不存在 —— 本检查空转了"
    fail=1
    return
  fi
  if ! grep -qE "struct[[:space:]]+$ty\b" "$file"; then
    echo "✗ $file 里找不到 struct $ty —— 类型改名了，本检查在查一个不存在的东西"
    fail=1
    return
  fi

  # impl Deref for X / impl std::ops::Deref for X，允许中间有泛型参数与空白
  if grep -nE "impl([[:space:]]*<[^>]*>)?[[:space:]]+(std::ops::)?Deref([[:space:]]*<[^>]*>)?[[:space:]]+for[[:space:]]+(&[[:space:]]*)?$ty\b" "$file"; then
    echo "✗ $ty 实现了 Deref（$src）"
    fail=1
  fi

  # Deref 之外的等价逃生口：任何返回 &PgPool 或 PgPool 的 pub 方法。
  # 放行项由调用方按目标传入，不能写死在这里：`AdminOnlyPool::raw` 获批的是
  # **测试侧那个**，生产侧的 AdminPool 一个都不许有（plan/00 §4.3 第 3 条）。
  # 写死成共用白名单的话，给 AdminPool 加一个同签名的 raw 会被静默放行。
  # 放行正则也必须带完整签名，不能写成「含 raw 就放行」—— 否则加个 raw_mut 就绕过去了。
  local leaks
  leaks="$(grep -nE 'pub[[:space:]]+(async[[:space:]]+)?fn[[:space:]]+[a-z_]+\(&?[[:space:]]*(mut[[:space:]]+)?self[^)]*\)[[:space:]]*->[[:space:]]*&?[[:space:]]*(sqlx::)?PgPool' "$file" \
    | { if [ -n "$allow" ]; then grep -vE "$allow"; else cat; fi; })"
  if [ -n "$leaks" ]; then
    echo "✗ $ty 开了 Deref 之外的池逃生口（$src）："
    printf '%s\n' "$leaks" | sed 's/^/       /'
    fail=1
  fi
}

# 自检先跑：这套 grep 必须能抓到它要抓的东西。
# 不做这一步的话，一个正则写错的版本会永远全绿 —— 与上面注释里那两条失败同形。
# 注意 check 在管道里跑，赋值落在子 shell，不会污染下面真实检查的 fail。
echo "══ 自检：造三个坏副本，必须都被抓到 ══"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
probe="$tmp/probe.rs"
cat > "$probe" <<'RS'
pub struct AdminPool(PgPool);
impl std::ops::Deref for AdminPool {
    type Target = PgPool;
    fn deref(&self) -> &PgPool { &self.0 }
}
RS
if check "$probe" AdminPool "自检" 2>&1 | grep -q '实现了 Deref'; then
  echo "✓ Deref 形状能被抓到"
else
  echo "✗ 自检失败：带 Deref 的副本没被抓到，上面的绿是空的"
  fail=1
fi

probe2="$tmp/probe2.rs"
cat > "$probe2" <<'RS'
pub struct AdminPool(PgPool);
impl AdminPool {
    pub fn inner(&self) -> &PgPool { &self.0 }
}
RS
if check "$probe2" AdminPool "自检" 2>&1 | grep -q '逃生口'; then
  echo "✓ 逃生口形状能被抓到"
else
  echo "✗ 自检失败：返回 &PgPool 的 pub 方法没被抓到"
  fail=1
fi

# 第三个探针钉的是白名单的作用域。白名单曾经写死在 check 内部，对两个被检文件
# 同样生效 —— 那时给 AdminPool 加一个同签名的 raw 会漏检（实测 fail=0），
# 而 AdminPool 恰恰是「不得开出取池路径」的那个。
probe3="$tmp/probe3.rs"
cat > "$probe3" <<'RS'
pub struct AdminPool(PgPool);
impl AdminPool {
    pub fn raw(&self) -> &PgPool { &self.0 }
}
RS
if check "$probe3" AdminPool "自检" 2>&1 | grep -q '逃生口'; then
  echo "✓ AdminPool 上的 raw 会被抓到（白名单只对测试侧那个生效）"
else
  echo "✗ 自检失败：AdminPool 上的 raw 被放行了 —— 白名单的作用域漏了"
  fail=1
fi

echo
echo "══ 两个池 newtype 的负向约束 ══"
# 第四个参数是获批的逃生口。AdminPool 传空：生产侧一条都不许有。
check crates/db/src/admin.rs         AdminPool     "plan/00 §4.3 第 3 条"
check crates/db/tests/support/mod.rs AdminOnlyPool "eng/03 §2.1" \
      'pub fn raw\(&self\) -> &PgPool'

echo
if [ $fail -eq 0 ]; then
  echo "通过：两个 newtype 都没有 Deref，也没有获批之外的取池路径。"
  echo
  echo "局限（不要把这条检查当成完整保证）："
  echo "  · 只查这两个文件。别处再包一个池它看不见"
  echo "  · 只查 Deref 与「返回 PgPool 的 pub 方法」两种形状。"
  echo "    通过 AsRef、Borrow、或一个自由函数把池取出来都绕得过去"
  echo "  · 是文本匹配而非类型检查。宏展开出来的 impl 查不到"
else
  echo "失败：见上面的 ✗。"
fi
exit $fail
