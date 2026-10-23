package com.policast.spark

import org.apache.spark.SparkContext
import org.apache.spark.api.plugin.{DriverPlugin, PluginContext}
import java.util.{Collections, Map => JMap}

/**
 * Driver-side plugin that loads the compiled policy manifest at Spark startup.
 *
 * The manifest path is configured via:
 *   spark.policast.manifest.path = /path/to/manifest.json
 *
 * The loaded manifest is stored as a local property so that Catalyst rules
 * can access it during query optimization.
 */
class PolicastDriverPlugin extends DriverPlugin {

  override def init(
      sc: SparkContext,
      pluginContext: PluginContext
  ): JMap[String, String] = {
    sc.getConf.getOption("spark.policast.manifest.path").filter(_.nonEmpty).foreach { path =>
      val manifest = PolicyManifest.load(path)
      val json = manifest.toJson
      sc.setLocalProperty(PolicastDriverPlugin.ManifestKey, json)
      sc.getConf.set(PolicastDriverPlugin.ManifestKey, json)
      logInfo(s"Policast: loaded ${manifest.policies.size()} policies from $path")
    }

    Collections.emptyMap()
  }

  private def logInfo(msg: String): Unit = {
    // Use Spark's internal logging when available, fall back to stderr
    System.err.println(s"[INFO] $msg")
  }

  private def logWarning(msg: String): Unit = {
    System.err.println(s"[WARN] $msg")
  }
}

object PolicastDriverPlugin {
  val ManifestKey = "policast.manifest.json"
}
