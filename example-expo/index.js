// example-expo/index.js
//
// Root bundle entry: Metro serves `index.bundle` to the dev client and the
// release bundle step starts here. It delegates to the configured `main`
// (`expo/AppEntry.js`), which registers the root `App` component. Without
// this file neither dev loading nor a release bundle can resolve the entry
// (finding FXM physical setup: `index.bundle` failed with
// UnableToResolveError before this file existed).
import 'expo/AppEntry'
