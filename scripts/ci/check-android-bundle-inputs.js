// Exercise the package-owned Gradle input registration, not a second cache model.
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '../..')
const java = path.join(process.env.JAVA_HOME || '', 'bin', process.platform === 'win32' ? 'java.exe' : 'java')
if (!process.env.JAVA_HOME || !fs.existsSync(java)) throw new Error('check-android-bundle-inputs requires JAVA_HOME')
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm bundle inputs '))
const copiedPackage = path.join(temp, 'copied package')
const moduleDir = path.join(copiedPackage, 'lib/module')
const application = path.join(temp, 'app')
const helper = path.join(root, 'android/bundle-js-inputs.gradle')
const groovyPath = value => value.replace(/\\/gu, '/').replace(/'/gu, "\\'")
const wrapper = path.join(root, 'example/android/gradle/wrapper/gradle-wrapper.jar')
function run() {
  const result = spawnSync(
    java,
    [
      '-cp',
      wrapper,
      'org.gradle.wrapper.GradleWrapperMain',
      '-p',
      temp,
      ':app:createBundleReleaseJsAndAssets',
      '--console=plain'
    ],
    { encoding: 'utf8', timeout: 120000 }
  )
  if (result.error || result.status !== 0)
    throw new Error(`Gradle fixture failed: ${result.error || result.status}\n${result.stdout}\n${result.stderr}`)
  return result.stdout
}
try {
  fs.mkdirSync(moduleDir, { recursive: true })
  fs.mkdirSync(application)
  fs.mkdirSync(path.join(copiedPackage, 'android'))
  fs.writeFileSync(path.join(copiedPackage, 'package.json'), '{"module":"lib/module/index.js"}')
  fs.writeFileSync(path.join(moduleDir, 'index.js'), 'first identity')
  fs.writeFileSync(path.join(application, 'app.js'), 'unchanged app')
  fs.writeFileSync(
    path.join(temp, 'settings.gradle'),
    `rootProject.name = 'ubm-bundle-input-fixture'
include ':library', ':app'
project(':library').projectDir = file('copied package/android')
`
  )
  fs.writeFileSync(path.join(temp, 'build.gradle'), '')
  // Before the fix, React Native's app-only input graph ignores node_modules.
  // An absent helper exercises that exact stale-output ordering for test RED.
  fs.writeFileSync(
    path.join(application, 'build.gradle'),
    `
apply plugin: 'base'
tasks.register('createBundleReleaseJsAndAssets') {
  inputs.file(file('app.js'))
  outputs.file(file('bundle.txt'))
  doLast {
    file('bundle.txt').text = fileTree('${groovyPath(moduleDir)}').matching { include '**/*.js' }.files.sort().collect { it.text }.join('|')
  }
}
`
  )
  // Execute the production root capture and public plugin hook, with Gradle's
  // lightweight base plugin standing in for Android. The library evaluates
  // before the app applies that plugin or registers its bundle task.
  const registration = fs
    .readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    .match(/def ubmJavaScriptPackageRoot = projectDir\.parentFile[\s\S]*?(?=\ndef appProject)/u)?.[0]
  fs.writeFileSync(
    path.join(copiedPackage, 'android/build.gradle'),
    fs.existsSync(helper)
      ? `apply from: '${groovyPath(helper)}'\n${registration.replace('com.android.application', 'base')}`
      : ''
  )
  run()
  assert.match(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  fs.writeFileSync(path.join(moduleDir, 'index.js'), 'second identity')
  const changed = run()
  assert.doesNotMatch(
    changed,
    /:createBundleReleaseJsAndAssets UP-TO-DATE/u,
    'copied library change must invalidate the bundle'
  )
  assert.equal(fs.readFileSync(path.join(application, 'bundle.txt'), 'utf8'), 'second identity')
  assert.match(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  fs.writeFileSync(path.join(moduleDir, 'added.js'), 'new module')
  assert.doesNotMatch(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  assert.equal(fs.readFileSync(path.join(application, 'bundle.txt'), 'utf8'), 'new module|second identity')
  fs.unlinkSync(path.join(moduleDir, 'added.js'))
  assert.doesNotMatch(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  assert.equal(fs.readFileSync(path.join(application, 'bundle.txt'), 'utf8'), 'second identity')
  fs.writeFileSync(path.join(copiedPackage, 'package.json'), '{"module":"lib/module/index.js","version":"changed"}')
  assert.doesNotMatch(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  assert.match(run(), /:createBundleReleaseJsAndAssets UP-TO-DATE/u)
  fs.renameSync(moduleDir, path.join(copiedPackage, 'removed-module'))
  assert.throws(run, /UBM bundle inputs require the installed package.json and lib\/module/u)
  console.log(
    'Android bundle inputs: copied JS modification/addition/removal and package metadata invalidate; unchanged inputs stay cached; paths with spaces work; missing copied modules fail closed'
  )
} finally {
  fs.rmSync(temp, { recursive: true, force: true })
}
